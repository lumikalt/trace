//! Inlining a call to a user `fn`/`impl` or the builtin `prio` — FIRRTL
//! has no call concept, so `compile_call`/`compile_builtin_call` splice
//! the callee in at its call site instead of emitting it as its own
//! hardware. `validate_call` is the single choke point every inlining
//! entry point (here, and writes.rs's write-hunt) goes through: effect
//! coloring, the call-site module boundary, and "does this body call a
//! user fn/impl anywhere" (which also rules out recursion — see
//! `compile_call`'s own doc comment). `compile_callee_body` builds the
//! callee's return value; `compile_prio` is the one synthesizable
//! builtin, a fixed-priority `mux` chain. See mod.rs's module doc
//! comment for the full v0 restriction this all enforces.

use super::Emitter;
use crate::ast::{Ast, Expr, ExprId, Item, ItemId, Param, Stmt, StmtId};
use crate::lexer::Span;
use crate::resolve::{DefId, DefKind, Resolution};

/// Whether any call to a user `fn`/`impl` appears anywhere in `id`'s
/// subtree — used to reject a callee whose own body calls something
/// else (see `Emitter::compile_call`'s doc comment for why that rules
/// out recursion too, not just deep inlining). A call to a BUILTIN
/// (`prio`, etc.) does NOT count: a builtin has no body of its own to
/// (re)inline, so it carries none of the reentrancy/recursion risk this
/// check exists to rule out — see `Emitter::compile_builtin_call`. Its
/// own arguments are still walked, though: `prio(SomeUserFn(x))` must
/// still disqualify on `SomeUserFn`.
pub(crate) fn expr_contains_call(ast: &Ast, res: &Resolution, id: ExprId) -> bool {
    match ast.expr(id) {
        Expr::Call { callee, args } => {
            let is_user_call = res
                .expr_defs
                .get(callee)
                .is_some_and(|d| matches!(res.def(*d).kind, DefKind::Fn | DefKind::Impl));
            is_user_call || args.iter().any(|a| expr_contains_call(ast, res, *a))
        }
        Expr::Ident(_) | Expr::Int(_) | Expr::Wildcard => false,
        Expr::Unary { operand, .. } => expr_contains_call(ast, res, *operand),
        Expr::Binary { lhs, rhs, .. } => {
            expr_contains_call(ast, res, *lhs) || expr_contains_call(ast, res, *rhs)
        }
        Expr::Guard(inner) | Expr::Spawn(inner) => expr_contains_call(ast, res, *inner),
        Expr::Field { base, .. } => expr_contains_call(ast, res, *base),
        Expr::Bracket { callee, args } => {
            expr_contains_call(ast, res, *callee)
                || args.iter().any(|a| expr_contains_call(ast, res, *a))
        }
    }
}

/// True if a callee's own body (or one of its `if`/`else` branches, same
/// shape) contains a call to a user `fn`/`impl` ANYWHERE — a
/// `let`/`return` expression, a write's RHS, an `if`'s condition, nested
/// arbitrarily deep through its own `if`/`else` (a call to a BUILTIN
/// does not count — see `expr_contains_call`). `validate_call` runs
/// this ONCE, on the whole body, as the single choke point every
/// inlining entry point (`compile_call`'s return-value splice,
/// `call_writes_reg`/`call_writes_port`'s write-hunt) goes through — so
/// a call buried only reachable through the write-hunt path (a
/// bare-statement call whose return value nothing ever asks for) still
/// gets this checked, not just the return-value path
/// `compile_callee_body` walks.
pub(crate) fn body_contains_call(ast: &Ast, res: &Resolution, stmts: &[StmtId]) -> bool {
    stmts.iter().any(|s| match ast.stmt(*s) {
        Stmt::Let { init, .. } => expr_contains_call(ast, res, *init),
        Stmt::Assign { lhs, rhs } => {
            expr_contains_call(ast, res, *lhs) || expr_contains_call(ast, res, *rhs)
        }
        Stmt::Return(Some(e)) => expr_contains_call(ast, res, *e),
        Stmt::Return(None) | Stmt::Tick => false,
        Stmt::Expr(e) => expr_contains_call(ast, res, *e),
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            expr_contains_call(ast, res, *cond)
                || body_contains_call(ast, res, then_body)
                || else_body
                    .as_ref()
                    .is_some_and(|b| body_contains_call(ast, res, b))
        }
        Stmt::While { cond, body } => {
            expr_contains_call(ast, res, *cond) || body_contains_call(ast, res, body)
        }
    })
}

/// Collects every `Expr::Call` reachable from `id`, including `id`
/// itself if it is one (and recursing into ITS OWN arguments too,
/// unlike `expr_contains_call`, which only needs to know one exists
/// anywhere). Used by `check_writing_call_positions` to find a
/// state-writing call hiding somewhere other than the two shapes
/// `call_writes_reg`/`call_writes_port` actually look for.
pub(crate) fn collect_calls(ast: &Ast, id: ExprId, out: &mut Vec<ExprId>) {
    match ast.expr(id) {
        Expr::Call { args, .. } => {
            out.push(id);
            for a in args {
                collect_calls(ast, *a, out);
            }
        }
        Expr::Ident(_) | Expr::Int(_) | Expr::Wildcard => {}
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
    ///   cheaper check than the signature one above), no further calls
    ///   anywhere in the tree (sidesteps recursion entirely: a function
    ///   whose own body cannot call anything can never call itself,
    ///   directly or through a cycle).
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
        // Checked ONCE, here, for the WHOLE body (including every branch
        // of an `if`/`else`), regardless of which entry point (return-
        // value splice or write-hunt) is asking: a callee whose body
        // cannot call anything can never call itself, directly or
        // through a cycle, so recursion needs no separate check. Without
        // this living at the shared choke point, a call reachable ONLY
        // through the write-hunt path (a bare-statement call whose
        // return value nothing ever uses) would skip it entirely —
        // `compile_callee_body`'s OWN version of this check only runs on
        // the return-value path, so a transitively-written register
        // nested two calls deep (`Outer` calls `Inner`, which writes
        // `w`; a rule calls `Outer` as a bare statement) would silently
        // vanish from the emitted hardware, with `effects.rs` still
        // correctly (and now misleadingly) telling schedule.rs that the
        // rule writes `w`.
        if body_contains_call(self.ast, self.res, &body) {
            self.error(
                span,
                "this function's body calls another function or builtin, which is not \
                 yet supported for inlining (v0 restriction: a called function's own \
                 body must not itself call anything, which also rules out recursion)"
                    .to_string(),
            );
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
            name => {
                self.error(
                    self.ast.expr_spans[id.0 as usize].clone(),
                    format!(
                        "calling the builtin `{name}` is not yet supported in FIRRTL \
                         emission (v0 restriction: only `prio` — a fixed-priority \
                         encoder — and `trunc` — bit truncation — are synthesizable \
                         today)"
                    ),
                );
                Err(())
            }
        }
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
        if !rest
            .iter()
            .all(|s| matches!(self.ast.stmt(*s), Stmt::Let { .. } | Stmt::Assign { .. }))
        {
            self.error(
                span.clone(),
                "this function's body is too complex to inline for its RETURN value \
                 (v0 restriction: only `let` bindings and state writes may come \
                 before a trailing `return`, or a trailing `if`/`else` whose \
                 branches both end that way — an `if`/`else` anywhere else, a loop, \
                 or a fifo/guard operation is not supported here). Called as a bare \
                 statement, with its return value unused, a conditional write like \
                 this one IS still supported — see `callee_reg_write`/\
                 `callee_port_write`, a separate walk that doesn't share this \
                 restriction"
                    .to_string(),
            );
            return Err(());
        }
        // A leading `Assign` is a state WRITE, a side effect this walk (which
        // only ever builds the RETURN value) doesn't care about — its own
        // value is found separately, by `callee_reg_write`/`callee_port_write`
        // when some register's/port's own write-threading walk reaches this
        // same call. (Whether this body calls anything at all was already
        // checked once, for the WHOLE body, by `validate_call` — the single
        // choke point every inlining entry point goes through, so this
        // doesn't need its own per-statement check.)

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
