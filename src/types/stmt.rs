//! Statement-level type checking: `type_stmt`'s per-`Stmt`-variant
//! dispatch (assignment, `let`, `if`/`if let`, `while`/`while let`,
//! `return`), the fallible-condition discharge rules an `if`/`while`
//! condition needs (`check_cond`, plus its undischarged-comparison
//! guard), state-write type checking (`type_write`), local rebinding's
//! width-widening (`widen`), and the width-fit/shape checks a write or
//! literal needs (`check_literal_fits`, `check_shift_amount`,
//! `check_assignable`).

use super::{Ty, TypeChecker, Width, bits_needed};
use crate::ast::{Expr, ExprId, Stmt, StmtId};
use crate::lexer::Span;
use crate::resolve::{DefId, DefKind, is_guard_like};
use std::collections::HashMap;

impl<'a> TypeChecker<'a> {
    pub(crate) fn type_stmt(
        &mut self,
        id: StmtId,
        locals: &mut HashMap<DefId, Ty>,
        ret: Option<&Ty>,
    ) {
        match self.ast.stmt(id).clone() {
            Stmt::Expr(e) => {
                // A bare expression left unused implicitly guards the
                // rule (`resolve::is_guard_like`) -- explicit `e?`
                // included, since `type_expr(Guard(inner))` already
                // passes through to `inner`'s own type -- so it gets
                // the same bits[1] enforcement an `if`/`while`
                // condition already gets. Anything else bare (a call,
                // fifo op, spawn) keeps its own independent type.
                // A bare COMPARISON is the one exception: like a fifo
                // op, it now has its OWN independent gating mechanism
                // (`compile_guard_unwrap_cond`, firrtl/writes.rs) that
                // doesn't route through this [1] requirement at all.
                // This is a RULE-BODY bare statement, not an if/while
                // condition -- `check_cond` below still rejects a bare
                // comparison for `while`, and for an `if` it's handled by
                // its own dedicated `Stmt::If` arm (which never reaches
                // this `Stmt::Expr` arm at all), not this path; `logic`
                // remains a valid, unaffected discharge everywhere.
                if matches!(self.ast.expr(e), Expr::Binary { op, .. } if op.is_comparison()) {
                    self.type_expr(e, locals);
                } else if is_guard_like(self.ast, self.res, e) {
                    self.check_cond(e, locals, false);
                } else {
                    self.type_expr(e, locals);
                }
            }
            Stmt::Assign { lhs, rhs } => {
                let rhs_ty = self.type_expr(rhs, locals);
                self.type_write(lhs, rhs_ty, rhs, locals);
            }
            Stmt::Let { name, init } => {
                let ty = self.type_expr(init, locals);
                for (i, d) in self.res.defs.iter().enumerate() {
                    if d.span == name.span {
                        locals.insert(DefId(i as u32), ty);
                        break;
                    }
                }
            }
            Stmt::Tick => {}
            Stmt::Break => {}
            Stmt::Return(Some(e)) => {
                let ty = self.type_expr(e, locals);
                if let Some(ret) = ret {
                    self.check_assignable(&ty, ret, self.expr_span(e), "return value");
                }
            }
            Stmt::Return(None) => {}
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                // `if`, unlike `while`, allows a BARE comparison directly
                // as its condition (Lumi's call: branch-scoped, Verse-
                // faithful semantics chosen uniformly for both the
                // with-else and no-else shapes -- see DESIGN.md's "`if`:
                // branch-scoped fallible conditions"). `logic` is no
                // longer required here, though it still works (`logic`'s
                // own type is plain `[1]`, unaffected by this exemption).
                self.check_cond(cond, locals, true);
                for s in then_body {
                    self.type_stmt(s, locals, ret);
                }
                for s in else_body.unwrap_or_default() {
                    self.type_stmt(s, locals, ret);
                }
            }
            Stmt::IfLet {
                name,
                init,
                then_body,
                else_body,
            } => {
                // `init` must be `inner?`, `inner : ?T` -- the v0-only
                // shape this feature discharges (a fifo op/failing call/
                // comparison as `init` is a clean, explicit rejection,
                // not a silent fallback to some other meaning). `type_
                // expr`'s own `Expr::Guard` arm already peels `?T` to `T`
                // (or passes through unchanged for any OTHER guard shape)
                // -- checking `inner`'s OWN type afterward, not just
                // matching the `Expr::Guard` AST shape, is what actually
                // distinguishes "was an Option" from "was a fifo op/
                // comparison/failing call that also happens to parse as
                // `Guard`".
                let bound_ty = if let Expr::Guard(inner) = self.ast.expr(init) {
                    let inner = *inner;
                    let ty = self.type_expr(init, locals);
                    if matches!(self.types.expr_tys.get(&inner), Some(Ty::Option(_))) {
                        ty
                    } else {
                        self.error(
                            self.expr_span(init),
                            "`if let`'s right-hand side must be an Option's own unwrap \
                             (`opt?`, `opt : ?T`) (a fifo op or failing call is fallible \
                             by default -- drop the `?` -- and a bare comparison isn't \
                             supported here yet, v0 restriction)"
                                .to_string(),
                        );
                        Ty::Unknown
                    }
                } else if self.is_fifo_deq(init) {
                    // `if let x = fifo.Deq[] { ... }` -- bare, never
                    // Guard-wrapped: `Deq[]` is fallible by default, same
                    // as a comparison, no `?` needed (see DESIGN.md's
                    // "`if let`: a fifo op's presence"). `type_expr`
                    // already dispatches a `Deq[]` bracket through `type_
                    // bracket`, which returns the fifo's own element type
                    // -- reused as-is, nothing fifo-specific to redo here.
                    self.type_expr(init, locals)
                } else if self.is_failing_call(init) {
                    // `if let x = Classify(a) { ... }` -- bare, same
                    // reasoning as the fifo case: a failing call is
                    // already fallible by default (`sig.fails`), no `?`
                    // needed. `type_expr` already dispatches a `Call`
                    // through `type_call`, which returns the callee's own
                    // return type -- reused as-is.
                    self.type_expr(init, locals)
                } else {
                    self.type_expr(init, locals);
                    self.error(
                        self.expr_span(init),
                        "`if let`'s right-hand side must be an Option's own unwrap \
                         (`opt?`, `opt : ?T`) -- missing the `?`?"
                            .to_string(),
                    );
                    Ty::Unknown
                };
                for (i, d) in self.res.defs.iter().enumerate() {
                    if d.span == name.span {
                        locals.insert(DefId(i as u32), bound_ty);
                        break;
                    }
                }
                for s in then_body {
                    self.type_stmt(s, locals, ret);
                }
                for s in else_body.unwrap_or_default() {
                    self.type_stmt(s, locals, ret);
                }
            }
            Stmt::While { cond, body } => {
                // `while COND { body }` renders (lower.rs's `while_loop_
                // header`) as literal `if COND { body; cont := 1 } else {
                // cont := 2 }` source text, re-fed through this ENTIRE
                // pipeline -- so a bare comparison/fifo-Deq/failing-call
                // here just needs to survive THIS check once, and the
                // rendered `if`'s own (already-built) exemption takes it
                // from there with zero new value-compilation code. `true`
                // here mirrors `if`'s own `allow_bare_comparison: true`
                // exactly (Lumi's call: "I still want while to work like
                // if, so it takes a fallible as a guard").
                self.check_cond(cond, locals, true);
                for s in body {
                    self.type_stmt(s, locals, ret);
                }
            }
            Stmt::WhileLet { name, init, body } => {
                // Same v0-restricted `Expr::Guard(inner)`-over-`Ty::
                // Option` shape `IfLet`'s own arm above requires, for the
                // identical reason -- `while let`'s WHOLE distinction from
                // `while` is that its own presence check is exactly this
                // Option-unwrap, not a general fallible condition (see
                // DESIGN.md's "`while`: multi-cycle loops").
                let bound_ty = if let Expr::Guard(inner) = self.ast.expr(init) {
                    let inner = *inner;
                    let ty = self.type_expr(init, locals);
                    if matches!(self.types.expr_tys.get(&inner), Some(Ty::Option(_))) {
                        ty
                    } else {
                        self.error(
                            self.expr_span(init),
                            "`while let`'s right-hand side must be an Option's own unwrap \
                             (`opt?`, `opt : ?T`) (v0 restriction: a fifo op, failing \
                             call, or comparison isn't supported here yet)"
                                .to_string(),
                        );
                        Ty::Unknown
                    }
                } else {
                    self.type_expr(init, locals);
                    self.error(
                        self.expr_span(init),
                        "`while let`'s right-hand side must be an Option's own unwrap \
                         (`opt?`, `opt : ?T`) -- missing the `?`?"
                            .to_string(),
                    );
                    Ty::Unknown
                };
                for (i, d) in self.res.defs.iter().enumerate() {
                    if d.span == name.span {
                        locals.insert(DefId(i as u32), bound_ty);
                        break;
                    }
                }
                for s in body {
                    self.type_stmt(s, locals, ret);
                }
            }
        }
    }

    fn check_cond(
        &mut self,
        cond: ExprId,
        locals: &mut HashMap<DefId, Ty>,
        allow_bare_comparison: bool,
    ) {
        let ty = self.type_expr(cond, locals);
        // A bare `opt?` statement (`opt : ?T`) unwraps-or-fails,
        // discarding the unwrapped value -- the same "gate the rule,
        // ignore the value" position a bare fifo-op statement already
        // occupies (`is_guard_like` excludes those from `check_cond`
        // entirely; a Guard-wrapped Option can't be excluded the same
        // way up front, since an ORDINARY `(cond)?` still needs this
        // check) -- so `T`'s own width is irrelevant here, unlike a
        // real condition.
        if let Expr::Guard(inner) = self.ast.expr(cond)
            && matches!(self.types.expr_tys.get(inner), Some(Ty::Option(_)))
        {
            return;
        }
        // `(a > b)?`: same idea, a comparison's own "value" (per
        // `type_binop`) is `a`'s type, not [1] -- explicitly gating on
        // it with `?` doesn't need it to ALSO be a real boolean here.
        // Unlike a BARE comparison (handled by the `allow_bare_comparison`
        // exemption just below, `if`-only), an EXPLICIT `?` reaching an
        // if/while condition is already rejected by `check_guard_
        // placement`'s position restriction regardless of this exemption,
        // so there's no silent-miscompile risk in exempting it here too.
        if let Expr::Guard(inner) = self.ast.expr(cond)
            && matches!(self.ast.expr(*inner), Expr::Binary { op, .. } if op.is_comparison())
        {
            return;
        }
        // A BARE comparison directly as an `if`'s own condition (no `?`,
        // no `logic`): the branch-scoped discharge this feature adds.
        // `while` never sets `allow_bare_comparison`, so its bare
        // comparison stays a type error same as before. Deliberately
        // narrow: only the WHOLE condition being a comparison qualifies,
        // not one nested inside a larger condition expression (`if (a >
        // b) & c`) -- see `expr_has_undischarged_comparison` just below
        // for why nesting is rejected outright rather than silently
        // accepted.
        if allow_bare_comparison
            && matches!(self.ast.expr(cond), Expr::Binary { op, .. } if op.is_comparison())
        {
            return;
        }
        // A bare fifo `Deq[]` or failing call directly as an `if`'s own
        // condition -- the same `if`-only discharge the bare-comparison
        // exemption just above gets, extended to the two OTHER fallible-
        // by-default shapes `if let`'s own arm already accepts (`is_fifo_
        // deq`/`is_failing_call`, expr.rs). `while` never sets
        // `allow_bare_comparison`, so stays restricted here too.
        if allow_bare_comparison && (self.is_fifo_deq(cond) || self.is_failing_call(cond)) {
            return;
        }
        // Advisor-caught, twice, while building the exemption just above:
        // a comparison's own type is its LHS's type (`type_binop`), so
        // once that LHS happens to be exactly 1 bit wide, a comparison
        // COMBINED with `&`/`|`/`^` (`(a > b) & c`, `a, b, c : [1]`) types
        // as a perfectly ordinary ty::Bits(1) -- indistinguishable from a
        // genuine boolean by the width check below alone. That's not
        // hypothetical: `if (a > b) & c` compiled clean pre-existing this
        // feature (`a01683f`, unrelated to the `allow_bare_comparison`
        // exemption above) to `mux(and(a, c), ...)`, silently using `a`'s
        // own passthrough value instead of `gt(a, b)` -- and the exact
        // same shape reaches a bare rule-body guard statement too
        // (`((a > b) & c)?` folded to `fires_r = and(a, c)`, dropping the
        // comparison's guard entirely), since `compile_guard_unwrap_cond`
        // (firrtl/writes.rs) only special-cases a comparison sitting
        // DIRECTLY as its own operand, the same shape this fn's own
        // exemptions above check for. `while x <> 0` with a 1-bit `x` had
        // the identical gap (no `allow_bare_comparison` needed to trigger
        // it at all). Reject outright, mirroring `contains_comparison`'s
        // (firrtl/checks.rs) body-nesting restriction and the "wrap it
        // with `logic`" precedent `logic A & logic B` already set --
        // don't silently fold, since finding-and-correctly-compiling a
        // comparison nested arbitrarily deep in a boolean combination
        // (unlike the guard-FOLD's own `comparison_conds`, which has no
        // VALUE to get wrong) would still leave the wrong VALUE reaching
        // this mux selector.
        if self.expr_has_undischarged_comparison(cond) {
            self.error(
                self.expr_span(cond),
                "a comparison combined with another condition (`&`/`|`/`^`, or nested \
                 inside a larger expression) is not yet supported here (v0 restriction): \
                 wrap it with `logic` first -- `(logic a > b) & c` -- or, for an `if`, \
                 use it as the WHOLE condition on its own"
                    .to_string(),
            );
            return;
        }
        // Anything reaching here didn't match any of the recognized
        // fallible shapes above (an explicit `<expr>?`, or -- if/while
        // only -- a bare comparison/fifo-op/failing-call as the WHOLE
        // condition). For an if/while condition specifically
        // (`allow_bare_comparison`), a plain `[1]` value is no longer
        // silently accepted here: Lumi's call, closing the gap where
        // `logic <fallible>` (already discharged into an ordinary,
        // no-longer-fallible bool) or a plain state read (`opt.valid`)
        // could sit in a guard-shaped position looking like a real guard
        // while actually just being an unconditional mux select. Applies
        // uniformly to rule AND callee (`fn`/`impl`) bodies alike --
        // `effects.rs`'s `infer_branch_scoped_cond` is what makes this
        // safe in a callee: an explicit `<expr>?` sitting in this exact
        // branch-scoped position is treated identically to a bare
        // failing call there (already exempt from forcing `<fails>`),
        // per Lumi's own insight that the two are the same shape of
        // thing. A bare rule-top-level guard STATEMENT (`allow_bare_
        // comparison: false`, `Stmt::Expr`'s own call site above) is
        // deliberately unaffected -- a different, pre-existing feature
        // (an ordinary `bits[1]` value already legitimately enables/
        // disables the whole rule there), not what this restriction
        // targets.
        //
        // A boolean COMBINATION of already-discharged fallibles -- `(logic
        // A) & (logic B)`, Verse's own `and` idiom (DESIGN.md's "`or`:
        // fallback chains") -- is a real, intentional escape hatch, not a
        // plain value read: an `Expr::Logic` reachable anywhere in
        // `cond`'s own subtree (but NOT `cond` itself, that bare shape is
        // the one this restriction specifically targets, see the hint
        // below) marks every component as deliberately discharged, same
        // `sub_exprs` walk `expr_has_undischarged_comparison` above
        // already uses for the identical reason.
        let contains_logic =
            !matches!(self.ast.expr(cond), Expr::Logic(_)) && contains_logic_expr(self.ast, cond);
        match ty {
            // `cond` itself being `Expr::Guard(_)` -- an explicit `<expr>?`
            // -- is the general fallible-marker escape hatch, regardless
            // of what's inside: the two exemptions above already `return`
            // early for the Option/comparison inner shapes (where `ty`
            // itself wouldn't even BE `[1]`), so reaching here with a
            // top-level `Guard` and a `[1]` `ty` means `?` wrapped
            // something whose own type already happens to be `[1]` (a
            // plain bit, a fifo op/failing call with a `[1]` element, an
            // already-`logic`-discharged bool) -- explicitly marked
            // fallible by the user, not silently inferred, so it's exempt
            // from the same reasoning as the two `return`s above, just
            // needing the ordinary width check instead of skipping it.
            Ty::Bits(Width::Known(1))
                if allow_bare_comparison
                    && !matches!(self.ast.expr(cond), Expr::Guard(_))
                    && !contains_logic =>
            {
                let hint = if matches!(self.ast.expr(cond), Expr::Logic(_)) {
                    "`logic` already discharges its operand into a plain boolean, \
                     which isn't a fallible expression on its own -- a bare `logic \
                     <expr>` condition here just becomes an ordinary (always-taken) \
                     mux select, not a real guard; drop `logic` if `<expr>` is \
                     itself a comparison/fifo op/failing call (already legal bare \
                     here), or keep `logic` and wrap the whole thing with `?` to \
                     make it fallible again; use `(logic <expr>)?`"
                } else {
                    "condition must be a fallible expression here -- a comparison, \
                     fifo op, failing call, or an explicit `<expr>?` -- not a plain \
                     `[1]` value; use `<expr>?`"
                };
                self.error(self.expr_span(cond), hint.to_string());
            }
            Ty::Bits(Width::Known(1)) | Ty::Bits(Width::Unknown) | Ty::Unknown | Ty::Int => {}
            other => self.error(
                self.expr_span(cond),
                format!("condition must be [1], got {other} (compare explicitly)"),
            ),
        }
    }

    /// Whether `id` IS an undischarged comparison, or has one reachable
    /// anywhere in its own subexpression tree -- `logic <comparison>`
    /// exempted (already discharged, see `logic`'s own entry in TODO.md),
    /// everything else walked generically via `sub_exprs` (lower.rs).
    /// `check_cond`'s own two exemptions above (an explicit `(a > b)?`,
    /// and -- `if`-only -- a bare comparison as the WHOLE condition) both
    /// `return` before reaching this check, so by the time this runs
    /// `cond` itself may still legitimately BE a bare comparison (a
    /// `while`'s, always rejected) or may legitimately CONTAIN a
    /// `logic`-discharged one (`(logic a > b) & c`, correctly exempted
    /// here) -- only a comparison neither wrapper has touched counts.
    fn expr_has_undischarged_comparison(&self, id: ExprId) -> bool {
        if let Expr::Logic(inner) = self.ast.expr(id)
            && matches!(self.ast.expr(*inner), Expr::Binary { op, .. } if op.is_comparison())
        {
            return false;
        }
        if matches!(self.ast.expr(id), Expr::Binary { op, .. } if op.is_comparison()) {
            return true;
        }
        crate::lower::sub_exprs(self.ast, id)
            .into_iter()
            .any(|child| self.expr_has_undischarged_comparison(child))
    }

    /// `lhs := rhs`: state writes check width; local (re)binds widen.
    fn type_write(
        &mut self,
        lhs: ExprId,
        rhs_ty: Ty,
        rhs: ExprId,
        locals: &mut HashMap<DefId, Ty>,
    ) {
        match self.ast.expr(lhs).clone() {
            Expr::Ident(_) => {
                let Some(def) = self.res.expr_defs.get(&lhs).copied() else {
                    return;
                };
                if let Some(state) = self.state_tys.get(&def).cloned() {
                    // `type_expr` is what normally populates `expr_tys` for
                    // every expression it types, but a write target never
                    // goes through it — `type_write` looks its type up
                    // directly from `state_tys`/`locals` instead, since a
                    // write has no "value" of its own to compute a `Ty`
                    // FROM the way a read does. Without this, hovering a
                    // reg/out/mem/fifo at its own `x := ...` write site
                    // (the language server's `thing_at` still finds the
                    // def fine, through `resolve.rs`'s own `expr_defs`
                    // insert for the LHS) found no entry here and silently
                    // showed no type — caught by hovering an `out` port at
                    // its write site specifically, but the gap was general
                    // to every write-target kind, not particular to `out`.
                    self.types.expr_tys.insert(lhs, state.clone());
                    self.check_assignable(&rhs_ty, &state, self.expr_span(rhs), "state write");
                    self.check_literal_fits(rhs, &state);
                    if matches!(state, Ty::Struct { .. })
                        && !matches!(
                            self.ast.expr(rhs),
                            Expr::StructLit { .. } | Expr::Call { .. }
                        )
                    {
                        self.error(
                            self.expr_span(rhs),
                            "a struct-typed write's right-hand side must be a struct \
                             literal (v0 restriction) -- copying one struct value into \
                             another isn't supported yet; construct a fresh literal \
                             instead"
                                .to_string(),
                        );
                    }
                    // Same restriction, `?T`'s own flavor: unlike a
                    // struct, an Option has no literal AST form of its
                    // own to require here (`false`, or any bare
                    // present-coerced value, both compile directly) —
                    // so this checks the RHS's TYPE instead of its AST
                    // shape. The one shape emission genuinely can't
                    // handle is another *same-shaped* `?T`-typed value
                    // (`opt2 := opt1`, both `?bits[8]`): there's no
                    // struct literal to decompose fields from, so it
                    // would otherwise silently leave the register frozen
                    // at its reset value. A DIFFERENTLY-shaped Option on
                    // the right (`??bits[8]` state, `?bits[8]` rhs) is
                    // already a plain type mismatch `check_assignable`
                    // above reports on its own -- checking `state ==
                    // rhs_ty` here (not just "both Option") avoids a
                    // second, misleadingly-worded "copy" error on top of
                    // that one (self-caught while probing `??T`: `oo :=
                    // inner` used to emit both).
                    if matches!(state, Ty::Option(_))
                        && state == rhs_ty
                        && !matches!(self.ast.expr(rhs), Expr::Call { .. })
                    {
                        self.error(
                            self.expr_span(rhs),
                            "a `?T`-typed write's right-hand side must be `false` or a \
                             plain value of the wrapped type (v0 restriction) -- copying \
                             one `?T` value into another isn't supported yet"
                                .to_string(),
                        );
                    }
                } else if self.res.def(def).kind == DefKind::Local {
                    let merged = match locals.get(&def) {
                        Some(old) => self.widen(old.clone(), rhs_ty, lhs),
                        None => rhs_ty,
                    };
                    self.types.expr_tys.insert(lhs, merged.clone());
                    locals.insert(def, merged);
                }
            }
            Expr::Bracket { callee, args } => {
                let base = self.type_expr(callee, locals);
                for a in &args {
                    self.type_expr(*a, locals);
                }
                match base {
                    Ty::Mem { elem, .. } => {
                        self.check_assignable(&rhs_ty, &elem, self.expr_span(rhs), "memory write");
                    }
                    Ty::Unknown => {}
                    other => self.error(
                        self.expr_span(lhs),
                        format!("cannot index-assign into {other}"),
                    ),
                }
            }
            Expr::Field { base, name } => {
                if let Some(module_def) = self.instance_module_of(base) {
                    // `base` is a valid instance reference in this
                    // `.port` position, not a bare value use — do not
                    // route through the generic Ident type check, which
                    // rejects a standalone instance reference.
                    self.types.expr_tys.insert(base, Ty::Unknown);
                    match self.find_port(module_def, &name) {
                        Some((DefKind::Input, port_ty)) => {
                            self.check_assignable(
                                &rhs_ty,
                                &port_ty,
                                self.expr_span(rhs),
                                "instance port write",
                            );
                            self.check_literal_fits(rhs, &port_ty);
                        }
                        Some((DefKind::Io, _)) => self.error(
                            self.expr_span(lhs),
                            format!(
                                "cannot write `{name}`: it is an io port on this instance \
                                 (io ports carry no value — the only legal use is `attach`ing \
                                 it to another io port)"
                            ),
                        ),
                        Some((_, _)) => self.error(
                            self.expr_span(lhs),
                            format!(
                                "cannot write `{name}`: it is an output port on this \
                                 instance (only input ports can be written)"
                            ),
                        ),
                        None => self.error(
                            self.expr_span(lhs),
                            format!("this instance has no port `{name}`"),
                        ),
                    }
                } else {
                    // Routes through the same read-side logic as any other
                    // field access (rejects a bogus field/base the same
                    // way `x.foo` would as an expression); a handle's or
                    // struct's fields additionally aren't writable at all
                    // (v0 restriction for structs: whole-value assignment
                    // only, `p := Pair{...}` — mirrors `Ty::Handle`'s own
                    // existing read-only restriction, same reasoning: no
                    // answer yet for what a partial-field write does to
                    // the OTHER fields of an if/else-nested assignment).
                    self.type_expr(lhs, locals);
                    match self.types.expr_tys.get(&base) {
                        Some(Ty::Handle(_)) => {
                            self.error(
                                self.expr_span(lhs),
                                format!("cannot write `.{name}`: a handle's fields are read-only"),
                            );
                        }
                        Some(Ty::Struct { .. }) => {
                            self.error(
                                self.expr_span(lhs),
                                format!(
                                    "cannot write `.{name}`: a struct's fields are read-only \
                                     (v0 restriction) — assign the whole value instead"
                                ),
                            );
                        }
                        Some(Ty::Option(_)) => {
                            self.error(
                                self.expr_span(lhs),
                                format!(
                                    "cannot write `.{name}`: a `?T` value's fields are \
                                     read-only — write the whole value instead (`false`, \
                                     or a plain value of the wrapped type)"
                                ),
                            );
                        }
                        _ => {}
                    }
                }
            }
            _ => {
                self.type_expr(lhs, locals);
            }
        }
    }

    /// Widening for rebound locals: same shape, width grows to max.
    fn widen(&mut self, old: Ty, new: Ty, at: ExprId) -> Ty {
        match (&old, &new) {
            (Ty::Unknown, _) => new,
            (_, Ty::Unknown) => old,
            (Ty::Bits(a), Ty::Bits(b)) => match (a, b) {
                (Width::Known(x), Width::Known(y)) => Ty::Bits(Width::Known(*x.max(y))),
                _ => Ty::Bits(Width::Unknown),
            },
            (Ty::Int, Ty::Int) => Ty::Int,
            (Ty::Int, Ty::Bits(_)) => new,
            (Ty::Bits(_), Ty::Int) => old,
            _ if old == new => old,
            _ => {
                self.error(
                    self.expr_span(at),
                    format!("rebinding changes type from {old} to {new}"),
                );
                new
            }
        }
    }

    /// Whether `id` reaches a `Pair{ ..base }`-shaped struct literal
    /// anywhere in its own tree — used to reject `..` in a reg/output
    /// INIT (a compile-TIME constant position, see `option_lit_field_
    /// const`/`struct_lit_field_const`, firrtl/mod.rs), where `base`
    /// would need to be a compile-time constant itself and generally
    /// isn't (a reg reference's flat fields aren't known until runtime).
    /// A walk, not a top-level-only check: `Outer{ inner: Inner{ ..old },
    /// x: 1 }` nests a `..` inside an explicitly-given field's own
    /// literal, still unreachable from a const-eval. Left unrejected,
    /// `struct_lit_field_const`'s existing `fields.iter().find(...)?`
    /// would return `None` for every field `..base` was meant to supply
    /// — routed by its caller (`module.rs`) through `.unwrap_or(0)`, a
    /// silent zero reset with no error at all, the exact same class of
    /// bug `optional opt1`'s aliasing rejection closed for `?T`.
    pub(crate) fn contains_struct_update(&self, id: ExprId) -> bool {
        if let Expr::StructLit { base: Some(_), .. } = self.ast.expr(id) {
            return true;
        }
        crate::lower::sub_exprs(self.ast, id)
            .into_iter()
            .any(|child| self.contains_struct_update(child))
    }

    /// A constant written into `[w]` must fit in `w` bits.
    pub(crate) fn check_literal_fits(&mut self, value: ExprId, target: &Ty) {
        if let Ty::Bits(Width::Known(w)) = target
            && let Some(v) = self.const_eval(value, &HashMap::new())
            && bits_needed(v) > *w
        {
            self.error(self.expr_span(value), format!("{v} does not fit in [{w}]"));
        }
    }

    /// A `where <ident> < <const>` bound's own base case: the declared
    /// init value must itself satisfy the bound, via the SAME
    /// `const_eval` `check_literal_fits` already uses. The parser only
    /// ever constructs this shape as `Binary { Lt, lhs, rhs }` (`where`
    /// hard-requires the literal `<` token, so no other comparison
    /// operator can reach here), and `resolve.rs` already requires
    /// `lhs` self-reference the declared reg — so the only thing left
    /// to verify here is the numeric relationship. An un-evaluable
    /// limit or init is ALSO an error, not silently skipped: the whole
    /// induction argument (`bounds.rs`) needs a verified starting
    /// point, and there's nothing to induct from otherwise.
    pub(crate) fn check_where_bound_init(&mut self, init: ExprId, bound: ExprId) {
        let Expr::Binary { rhs, .. } = self.ast.expr(bound).clone() else {
            return;
        };
        let Some(limit) = self.const_eval(rhs, &HashMap::new()) else {
            self.error(
                self.expr_span(rhs),
                "a `where` bound's own limit must be a compile-time constant".to_string(),
            );
            return;
        };
        let Some(v) = self.const_eval(init, &HashMap::new()) else {
            self.error(
                self.expr_span(init),
                "a `where`-bounded reg's init value must be a compile-time constant, to \
                 verify it satisfies the declared bound"
                    .to_string(),
            );
            return;
        };
        if v >= limit {
            self.error(
                self.expr_span(init),
                format!("init value {v} does not satisfy the declared bound (`< {limit}`)"),
            );
        }
    }

    /// A constant shift amount `>= w` discards every bit of a `[w]`
    /// operand — `Shr`/`AShr` always land on all-zero (or all-sign, for
    /// `AShr`), and `Shl` shifts every original bit out past the top
    /// (the result stays `[w]` wide, per `type_binop`'s own `Shl | Shr |
    /// AShr => Ty::Bits(a)` rule, not `[w + amount]`) — so a shift this
    /// large is near-certainly a bug (a swapped operand order, a typo),
    /// the same class `check_literal_fits` already catches for ordinary
    /// arithmetic. Deliberately separate from `check_literal_fits`
    /// rather than reusing it: a shift AMOUNT isn't a value bounded by
    /// the shifted operand's own width domain (`x >> 6` is fine on a
    /// `[4]` `x`, "6 does not fit in [4]" would be simply wrong), so
    /// this checks against a different bound (`>= w`, not "needs more
    /// than `w` bits to represent").
    pub(crate) fn check_shift_amount(&mut self, amount: ExprId, shifted: &Ty) {
        if let Ty::Bits(Width::Known(w)) = shifted
            && let Some(n) = self.const_eval(amount, &HashMap::new())
            && n >= *w
        {
            self.error(
                self.expr_span(amount),
                format!("shift by {n} discards every bit of a [{w}] value"),
            );
        }
    }

    /// May `value` be written where `target` is expected? Shapes must
    /// match; a known-wider value needs an explicit `trunc`.
    pub(crate) fn check_assignable(&mut self, value: &Ty, target: &Ty, span: Span, what: &str) {
        match (value, target) {
            (Ty::Unknown, _) | (_, Ty::Unknown) => {}
            (Ty::Int, Ty::Bits(_)) => {} // literal absorbs; range-checked at coercion
            (Ty::Bits(wv), Ty::Bits(wt)) => {
                if let (Width::Known(v), Width::Known(t)) = (wv, wt)
                    && v > t
                {
                    self.error(
                        span,
                        format!(
                            "{what} would silently truncate [{v}] to [{t}]; \
                             use `trunc(value, {t})`"
                        ),
                    );
                }
            }
            // `false` constructs the absent value of any `?T`.
            (Ty::AbsentLit, Ty::Option(_)) => {}
            // `optional e` forces exactly the NEXT layer's `valid` to
            // true, then recurses on `e`'s own type (looked up now that
            // it's needed, not precomputed — see `Ty::Optional`'s doc
            // comment) against the TARGET's inner. Checked ahead of the
            // generic bare-value-coercion arm below, whose guard would
            // otherwise accept a `Ty::Optional` value too (it isn't a
            // `Ty::Option`) and keep re-checking the SAME sentinel
            // against successively peeled targets instead of ever
            // unwrapping to `e`.
            (Ty::Optional(inner), Ty::Option(t_inner)) => {
                let inner_ty = self
                    .types
                    .expr_tys
                    .get(inner)
                    .cloned()
                    .unwrap_or(Ty::Unknown);
                // `optional <alias>`, `<alias>` a plain reference already
                // typed `?T` (a reg/param/local/field, not a fresh
                // literal/computed value) — the exact "copy one `?T`
                // value into another" v0 restriction below, one layer up.
                // Emission (`compile_field_path_value`, writes.rs) has no
                // way to thread an ALIASED `?T`'s own live valid/data
                // pair into another Option's flat fields; without this
                // check it silently falls through to that fn's existing
                // aliasing guard, which returns `None` for a value that
                // nothing escalates to a diagnostic on the WRITE side
                // (unlike the read side's `compile_struct_field_read`) —
                // the register would silently hold its previous value
                // instead of tracking `alias`. A CALL returning `?T` is
                // exempt: its return decomposes per-leaf
                // (`compile_call_field_value`) rather than aliasing a
                // flat register, the same exemption `type_write`'s own
                // Option-to-Option rejection already carves out.
                if matches!(inner_ty, Ty::Option(_))
                    && !matches!(self.ast.expr(*inner), Expr::Call { .. })
                {
                    self.error(
                        span,
                        format!(
                            "{what}: `optional` cannot wrap an existing `?T` value directly \
                             (v0 restriction) -- copying one `?T` value into another isn't \
                             supported yet; use a fresh value, `false`, or a nested `optional` \
                             instead"
                        ),
                    );
                    return;
                }
                self.check_assignable(&inner_ty, t_inner, span, what);
            }
            // A bare `T`-shaped value implicitly wraps into `?T` present
            // — reached anywhere `check_assignable` already runs (state
            // writes/inits, return values, call arguments, memory/
            // instance-port writes, struct-literal fields, ...), not
            // hand-restricted to a narrower set of positions. Recurses
            // rather than requiring exact equality, so a too-wide
            // literal into `?bits[8]` still gets the ordinary `trunc`
            // guidance instead of a generic mismatch. Guarded off
            // `value` itself being `Ty::Option` so writing one Option
            // value into another (`opt2 := opt1`) is NOT silently
            // treated as "present, holding an Option" — it falls
            // through to ordinary equality below instead, matching
            // `Ty::Struct`'s own copy-between-two-values handling.
            (_, Ty::Option(inner)) if !matches!(value, Ty::Option(_)) => {
                self.check_assignable(value, inner, span, what);
            }
            _ if value == target => {}
            _ => self.error(span, format!("{what}: expected {target}, got {value}")),
        }
    }
}

/// Whether an `Expr::Logic` node is reachable anywhere within `id`'s own
/// subexpression tree (`id` itself included) -- used by `check_cond` to
/// recognize a boolean COMBINATION of already-discharged fallibles
/// (`(logic A) & (logic B)`) as a legitimate if/while condition, not a
/// plain, un-provenanced `[1]` value. A free fn, not a `TypeChecker`
/// method, since it needs only `ast` -- mirrors `sub_exprs` (lower.rs)
/// itself in that respect.
fn contains_logic_expr(ast: &crate::ast::Ast, id: ExprId) -> bool {
    if matches!(ast.expr(id), Expr::Logic(_)) {
        return true;
    }
    crate::lower::sub_exprs(ast, id)
        .into_iter()
        .any(|child| contains_logic_expr(ast, child))
}
