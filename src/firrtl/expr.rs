//! The generic expression compiler (`compile_expr`/`compile_expr_hinted`):
//! identifiers, literals, arithmetic/bitwise/shift/comparison operators,
//! bit-select/slice, memory reads, `instance.port` reads — the "expression
//! surface" mod.rs's module doc comment enumerates. Dispatches out to
//! calls.rs for `Expr::Call`; everything else's FIRRTL text is built
//! directly here.

use super::Emitter;
use super::fifo::*;
use crate::ast::{BinOp, Expr, ExprId, Item, UnOp};
use crate::resolve::{DefId, DefKind};
use crate::types::{Ty, Width};
use std::collections::HashMap;

impl<'a> Emitter<'a> {
    /// Top-level entry: no width hint, so a bare literal falls back to
    /// its own (usually absent) type. Prefer `compile_expr_hinted` from
    /// any caller that knows the width the literal should take on —
    /// which is everywhere a literal can legally appear, since our type
    /// checker never gives a literal its own concrete width (it only
    /// *absorbs* one from its context).
    pub(crate) fn compile_expr(&mut self, id: ExprId) -> Result<String, ()> {
        self.compile_expr_hinted(id, None)
    }

    pub(crate) fn compile_expr_hinted(
        &mut self,
        id: ExprId,
        hint: Option<u64>,
    ) -> Result<String, ()> {
        if let Some((port, ..)) = self.read_ports.get(&id) {
            let Expr::Bracket { callee, .. } = self.ast.expr(id) else {
                unreachable!()
            };
            let Expr::Ident(mem_name) = self.ast.expr(*callee) else {
                unreachable!()
            };
            return Ok(format!("{mem_name}.{port}.data"));
        }
        if let Some((fifo, depth, is_enq, _)) = self.fifo_op(id)
            && !is_enq
        {
            return Ok(fifo_deq_read_expr(&fifo, depth));
        }
        // `inst.port` reading a child's output port: a plain combinational
        // reference, always valid (the child drives it unconditionally),
        // no gating needed — direction is already checked by types.rs.
        if let Expr::Field { base, name } = self.ast.expr(id).clone()
            && let Some(def) = self.res.expr_defs.get(&base)
            && self.res.def(*def).kind == DefKind::Inst
        {
            let inst_name = self.res.def(*def).name.clone();
            return Ok(format!("{inst_name}.{name}"));
        }
        // `p.field` where `p` is struct-typed: a reg/output/input's field
        // compiles to its own flat register/port (`{base}_{field}`,
        // confirmed via real firtool to match how a native FIRRTL bundle
        // flattens on its own — module.rs's Reg/Input/Output collection
        // already named the backing storage this same way). A struct-
        // typed LOCAL's field substitutes directly from its bound struct
        // literal instead: it never reaches `locals_snapshots`'s pre-
        // compiled-text path (`local_hint`, writes.rs, returns `None` for
        // any non-`bits[N]` local type, keeping it in the lazy
        // `self.locals` `ExprId` map, exactly what per-field extraction
        // needs).
        if let Expr::Field { base, name } = self.ast.expr(id).clone()
            && matches!(
                self.types.expr_tys.get(&base),
                Some(Ty::Struct { .. } | Ty::Option(_))
            )
        {
            let (root, mut path) = self.struct_field_path(base);
            path.push(name);
            return self.compile_struct_field_read(id, root, &path, hint);
        }
        match self.ast.expr(id).clone() {
            Expr::Ident(_) => {
                let def = self.res.expr_defs.get(&id).copied();
                match def.map(|d| self.res.def(d).clone()) {
                    Some(d) if d.kind == DefKind::Output => Ok(self.output_regs[&d.name].clone()),
                    Some(d) if matches!(d.kind, DefKind::Local | DefKind::Param) => {
                        let def = def.unwrap();
                        // `if let x = opt? { ... }`'s own `x`: resolves
                        // to `opt.data` -- reuses the exact same struct-
                        // field chase-through a plain `opt.data` field
                        // read already goes through (`struct_field_path`
                        // handles `opt` itself being a chained field,
                        // e.g. `frame.maybe?`), just reached from a bare
                        // Ident instead of an explicit `Expr::Field`.
                        // Checked BEFORE `locals_snapshots`/`locals`
                        // below: the two are mutually exclusive by
                        // construction (see `if_let_binds`'s own doc
                        // comment, mod.rs), so order between them and
                        // this check doesn't matter for correctness, but
                        // checking here first avoids a wasted snapshot
                        // lookup for every if-let-bound reference.
                        //
                        // `if let x = fifo.Deq[] { ... }` / `if let x =
                        // Classify(a) { ... }`'s own `x`: the bound expr
                        // is the bare Deq/call itself, not Option-wrapped
                        // -- `compile_expr_hinted`'s own top-of-function
                        // fifo-op check (above) and its generic `Expr::
                        // Call` dispatch (below, to `compile_call`) both
                        // already compile either shape to its own value
                        // generically, the same path an ordinary top-
                        // level `x := f.Deq[]`/`x := Classify(a)` goes
                        // through, so recursing into it here is enough,
                        // no `.data` chase needed.
                        if let Some(opt) = self.if_let_binds.get(&def).copied() {
                            if self.fifo_op(opt).is_some()
                                || matches!(self.ast.expr(opt), Expr::Call { .. })
                            {
                                return self.compile_expr_hinted(opt, hint);
                            }
                            let (root, mut path) = self.struct_field_path(opt);
                            path.push("data".to_string());
                            return self.compile_struct_field_read(id, root, &path, hint);
                        }
                        if let Some(text) = self
                            .locals_snapshots
                            .get(self.current_pos)
                            .and_then(|m| m.get(&def))
                        {
                            return Ok(text.clone());
                        }
                        match self.locals.get(&def) {
                            Some(bound) => self.compile_expr_hinted(*bound, hint),
                            None => {
                                self.error(
                                    self.ast.expr_spans[id.0 as usize].clone(),
                                    "cannot find this local's binding in the rule \
                                     currently being compiled (v0 restriction: a local \
                                     or a called function's parameter is only resolved \
                                     within its own rule/call)"
                                        .to_string(),
                                );
                                Err(())
                            }
                        }
                    }
                    Some(d) if matches!(d.kind, DefKind::Reg | DefKind::Input) => Ok(d.name),
                    _ => {
                        self.error(
                            self.ast.expr_spans[id.0 as usize].clone(),
                            "unsupported reference in FIRRTL emission (v0 restriction)".to_string(),
                        );
                        Err(())
                    }
                }
            }
            Expr::Int(v) => {
                let w = hint.unwrap_or_else(|| self.width_of(id));
                Ok(format!("UInt<{w}>({})", mask_to_width(v, w)))
            }
            // Unlike a bare `Int`, this already has its own definite
            // width (types.rs types it as `Bits(Known(width))`, not the
            // "absorb from context" `Ty::Int`) — so, like `compile_prio`/
            // `compile_trunc`/`compile_pack`'s own output, it ignores any
            // external hint and always emits at its own declared width.
            // A width mismatch against its context (e.g. `x + 8'd6` where
            // `x` is wider) is exactly the same "mismatched-width Bits
            // operands" case two real registers of different widths
            // already produce (`type_binop`'s `Width::Known(x.max(y))`
            // rule) — FIRRTL's own primops (`add`, etc.) already handle
            // that, the same way `tail(add(l, r), 1)` already trusts them
            // to for any two differently-sized real operands.
            Expr::SizedInt { width, value } => {
                Ok(format!("UInt<{width}>({})", mask_to_width(value, width)))
            }
            // A comparison used directly as a VALUE (not through `logic`,
            // which calls `compile_binop` straight — see its own doc
            // comment, calls.rs) yields `lhs`'s own value on success,
            // per `type_binop`'s matching rule (types.rs) — the same
            // "unwrap, don't discharge" shape a bare `f.Deq[]`/`opt?`
            // used as a value already has. The comparison ITSELF (its
            // ordinary `eq`/`neq`/`lt`/... boolean) is `compile_guard`'s
            // job (writes.rs), which folds it into the rule's own guard
            // — this fn only ever computes the VALUE half.
            Expr::Binary { op, lhs, .. } if op.is_comparison() => {
                self.compile_expr_hinted(lhs, hint)
            }
            Expr::Binary { op, lhs, rhs } => self.compile_binop(id, op, lhs, rhs),
            Expr::Unary { op, operand } => self.compile_unop(id, op, operand),
            Expr::Bracket { callee, args } => {
                if matches!(self.types.expr_tys.get(&callee), Some(Ty::Bits(_))) {
                    self.compile_bit_select(id, callee, &args)
                } else {
                    self.error(
                        self.ast.expr_spans[id.0 as usize].clone(),
                        "this indexing form is not yet supported in FIRRTL emission (v0 \
                         restriction: only memory reads and bit-select/slice on a plain \
                         identifier)"
                            .to_string(),
                    );
                    Err(())
                }
            }
            Expr::Call { callee, args } => {
                match self
                    .res
                    .expr_defs
                    .get(&callee)
                    .map(|d| self.res.def(*d).kind)
                {
                    Some(DefKind::Fn | DefKind::Impl) => self.compile_call(id, callee, &args, hint),
                    Some(DefKind::Builtin) => self.compile_builtin_call(id, callee, &args, hint),
                    _ => {
                        self.error(
                            self.ast.expr_spans[id.0 as usize].clone(),
                            "this call is not yet supported in FIRRTL emission (v0 \
                             restriction: only a call to a user `fn`/`impl` with a \
                             simple body — `let` bindings, `if`/`else` branches, a \
                             trailing `return`, and no call cycles — can be inlined; \
                             or a call to a builtin like `prio`)"
                                .to_string(),
                        );
                        Err(())
                    }
                }
            }
            Expr::Or(alts) => self.compile_or(id, &alts, hint),
            // `opt?`, `opt : ?T`, used as a VALUE (not a bare-statement
            // guard, which `compile_guard`/writes.rs folds separately):
            // its compiled value is the unwrapped `T`, i.e. `inner`'s own
            // `data` field — reusing the exact same peeling/dispatch
            // `.field` access already has (`inner` may itself be a
            // chained struct field, e.g. `frame.maybe_thing?`). An
            // ordinary bits[1] guard (`(cond)?`) used as a value just
            // passes `inner`'s own compiled value through unchanged —
            // types.rs's `Expr::Guard` arm does the identical passthrough
            // at the type level; the FAILURE side of either case is
            // entirely `compile_guard`'s job, not this fn's.
            Expr::Guard(inner) => {
                if matches!(self.types.expr_tys.get(&inner), Some(Ty::Option(_))) {
                    let (root, mut path) = self.struct_field_path(inner);
                    path.push("data".to_string());
                    self.compile_struct_field_read(id, root, &path, hint)
                } else {
                    self.compile_expr_hinted(inner, hint)
                }
            }
            Expr::Logic(inner) => self.compile_logic(id, inner),
            _ => {
                self.error(
                    self.ast.expr_spans[id.0 as usize].clone(),
                    "this expression form is not yet supported in FIRRTL emission (v0 \
                     restriction: identifiers, integers, arithmetic/bitwise/shift \
                     operators, comparisons, unary -/~, bit-select/slice, and memory \
                     reads only)"
                        .to_string(),
                );
                Err(())
            }
        }
    }

    /// `A or B or C`'s VALUE: a priority mux over each Deq alternative's
    /// (already-read, pre-edge) data, falling back to the default when
    /// present or a defined-but-meaningless `0` when not (never actually
    /// selected there — the rule's own guard already requires at least
    /// one alternative ready when there's no default, same convention
    /// `__race_value`'s own dead mux branch uses). Shape violations
    /// (an alt that isn't `Deq[]`, `Enq` used as an alt, depth>1) are
    /// `check_or_shape`'s job (checks.rs) and already turned into a
    /// compile error before this ever runs; `classify_or_alts` (fifo.rs)
    /// is the SAME default/alternative split `fifo.rs`'s `or_chains`
    /// uses for the guard fold (`compile_guard`, this file's sibling in
    /// writes.rs) and `rule_fifo_ops`'s `select` computation, so all
    /// three agree on what counts as a default without re-deriving it
    /// independently. Priority order matches `rule_fifo_ops`'s own
    /// `select` computation exactly — hand-lowered and Icarus-confirmed
    /// (fifo.rs's `or_chains` doc comment) before either was written.
    fn compile_or(&mut self, id: ExprId, alts: &[ExprId], hint: Option<u64>) -> Result<String, ()> {
        let (fifo_alts, default) = classify_or_alts(self.ast, self.res, alts);
        let w = hint.unwrap_or_else(|| self.width_of(id));
        let mut picks: Vec<(String, String)> = Vec::new();
        let mut none_selected: Option<String> = None;
        for alt in fifo_alts {
            let Some((fifo, depth, is_enq, _)) = self.fifo_op(alt) else {
                return Err(());
            };
            if is_enq {
                return Err(());
            }
            let own = fifo_guard_cond(&fifo, false, depth);
            let value = fifo_deq_read_expr(&fifo, depth);
            let select = match &none_selected {
                None => own.clone(),
                Some(ns) => format!("and({own}, {ns})"),
            };
            none_selected = Some(match &none_selected {
                None => format!("not({own})"),
                Some(ns) => format!("and({ns}, not({own}))"),
            });
            picks.push((select, value));
        }
        let mut out = match default {
            Some(expr) => self.compile_expr_hinted(expr, Some(w))?,
            None => format!("UInt<{w}>(0)"),
        };
        for (select, value) in picks.into_iter().rev() {
            out = format!("mux({select}, {value}, {out})");
        }
        Ok(out)
    }

    /// Peels a chain of struct-typed `.field` accesses down to its root
    /// expression, collecting the field-name chain in root-to-leaf order
    /// (`p.inner.a`'s `base` — `p.inner` — peels to `(p, ["inner"])`;
    /// the caller pushes the outer access's own field name, `"a"`, on
    /// top). Walks through a base that is ITSELF a struct-typed `Field`
    /// access, OR an Option-typed `Guard` (`a?.b`'s `base` is `Guard(a)`,
    /// `a : ?T` — the `?.` chain's own spine, see `lower::guard_chain_
    /// spine`'s doc comment: unwrapping doesn't move data around, `T`'s
    /// flattened registers ARE `?T`'s own `data.*`, so a `Guard` on the
    /// spine just pushes `"data"` the same way `compile_expr_hinted`'s
    /// own un-chained `Expr::Guard` value arm already does for the
    /// single-hop case — generalized here so it composes at any depth,
    /// `a?.b?.c` included). A plain struct-typed root value (an `Ident`)
    /// stops the walk, becoming `root` with an empty path so far.
    pub(crate) fn struct_field_path(&self, id: ExprId) -> (ExprId, Vec<String>) {
        if let Expr::Field { base, name } = self.ast.expr(id).clone()
            && matches!(
                self.types.expr_tys.get(&base),
                Some(Ty::Struct { .. } | Ty::Option(_))
            )
        {
            let (root, mut path) = self.struct_field_path(base);
            path.push(name);
            (root, path)
        } else if let Expr::Guard(inner) = self.ast.expr(id).clone()
            && matches!(self.types.expr_tys.get(&inner), Some(Ty::Option(_)))
        {
            let (root, mut path) = self.struct_field_path(inner);
            path.push("data".to_string());
            (root, path)
        } else {
            (id, Vec::new())
        }
    }

    /// `p.a.b. ...`'s value: `id` is the WHOLE field-access expression
    /// (used only as a width fallback when `hint` is absent), `root` the
    /// struct-/Option-typed value the chain starts from (already
    /// confirmed by the caller), `path` the field-name chain from
    /// `root`'s own top level down to the leaf field actually being read
    /// (`["inner", "a"]` for `p.inner.a`). Three cases, matching the
    /// plain-`Ident` dispatch's own split (`Ident` arm, above) one level
    /// deeper:
    /// - `Output`: its field's backing register (`__out_{name}_{path
    ///   joined with _}`, module.rs's own naming).
    /// - `Reg`/`Input`: its field's flat register/port (`{name}_{path
    ///   joined with _}`).
    /// - `Local`/`Param`: substitute the local's bound value and walk
    ///   `path` into it via `compile_field_path_value` (shared with
    ///   `writes.rs`'s per-field write-threading — a struct-typed
    ///   local's field read walks the same literal shape a struct-typed
    ///   reg/output's WRITE does; an Option-typed local's synthesizes
    ///   `valid`/`data` from `false`/a coerced value the same way). A
    ///   struct-typed local carries a real, if narrow, v0 restriction:
    ///   only a local bound DIRECTLY to a struct literal resolves (no
    ///   aliasing chain, e.g. `let q = p`) — `compile_field_path_value`
    ///   returns `None` for anything else, reported here. An Option-
    ///   typed local carries the IDENTICAL restriction (`let o = opt`,
    ///   `opt` itself `?T`, is rejected the same way) — a plain `let`
    ///   has no target type, so `type_write`'s Option-to-Option
    ///   rejection (the check that keeps `compile_field_path_value`'s
    ///   Option arm's "not itself Option-typed" invariant true
    ///   everywhere else) never runs for it, and without a matching
    ///   guard INSIDE that arm this silently hardcoded a wrong constant
    ///   instead of erroring — self-caught while investigating `?T`-
    ///   typed fn params, which bind an argument through this exact
    ///   path too.
    pub(crate) fn compile_struct_field_read(
        &mut self,
        id: ExprId,
        root: ExprId,
        path: &[String],
        hint: Option<u64>,
    ) -> Result<String, ()> {
        let def = self.res.expr_defs.get(&root).copied();
        let suffix = path.join("_");
        match def.map(|d| self.res.def(d).clone()) {
            Some(d) if d.kind == DefKind::Output => Ok(format!("__out_{}_{suffix}", d.name)),
            Some(d) if matches!(d.kind, DefKind::Reg | DefKind::Input) => {
                Ok(format!("{}_{suffix}", d.name))
            }
            Some(d) if matches!(d.kind, DefKind::Local | DefKind::Param) => {
                let def = def.unwrap();
                let is_param = d.kind == DefKind::Param;
                let Some(bound) = self.locals.get(&def).copied() else {
                    self.error(
                        self.ast.expr_spans[root.0 as usize].clone(),
                        "cannot find this local's binding in the rule currently being \
                         compiled (v0 restriction: a local is only resolved within its \
                         own rule/call)"
                            .to_string(),
                    );
                    return Err(());
                };
                let Some(root_ty) = self.types.expr_tys.get(&root).cloned() else {
                    self.error(
                        self.ast.expr_spans[root.0 as usize].clone(),
                        "unsupported struct reference in FIRRTL emission (v0 restriction)"
                            .to_string(),
                    );
                    return Err(());
                };
                // A PARAM bound to another struct/Option-typed value via
                // a plain `Expr::Ident` (a reg/output/input, or another
                // param/local of the SAME type -- `UseIt(q)`, `q` a
                // struct-typed reg) resolves by chasing through to that
                // value's own root, reusing this same fn one level
                // deeper -- exactly the substitution a real inliner
                // performs, and the actually-useful case ("pass a reg
                // as an argument"), not just a literal argument.
                // Deliberately PARAM-only, not LOCAL: a plain rule-level
                // `let` aliasing another struct/Option value stays
                // rejected (`option_typed_local_aliasing_another_
                // option_value_is_rejected`) -- general alias
                // resolution for `let` is a separate, wider capability
                // than "params work," not implied by it. The type-
                // equality check (not just "both struct"/"both Option")
                // excludes the coercion case: a plain `T`-typed
                // value/local passed to a `?T` param has a DIFFERENT
                // type from `root_ty` and correctly falls through to
                // `compile_field_path_value`'s coercion-synthesis path
                // below instead.
                if is_param
                    && matches!(self.ast.expr(bound), Expr::Ident(_))
                    && self.types.expr_tys.get(&bound) == Some(&root_ty)
                {
                    // `bound` chases to an `if let NAME = opt?`-bound
                    // name, passed WHOLE as this call's argument
                    // (`Get(v)`) rather than field-accessed directly
                    // (`v.x`, which never reaches here: that has `root`
                    // = `v` itself, a Local, so `is_param` above is
                    // false and it falls to the "cannot find this
                    // local's binding" rejection below instead, exactly
                    // as the deferred `.field` chase-through restriction
                    // requires). Recursing with `bound` as the new root
                    // would re-enter this match on `v`'s OWN DefId and
                    // hit that same rejection -- so jump straight to
                    // `opt`'s own root with `"data"` and the remaining
                    // `path` spliced on, the same rewrite `compile_expr_
                    // hinted`'s `if_let_binds` check performs for a
                    // whole-value (empty-path) read.
                    if let Some(bound_def) = self.res.expr_defs.get(&bound).copied()
                        && let Some(&opt) = self.if_let_binds.get(&bound_def)
                    {
                        let (opt_root, mut opt_path) = self.struct_field_path(opt);
                        opt_path.push("data".to_string());
                        opt_path.extend_from_slice(path);
                        return self.compile_struct_field_read(id, opt_root, &opt_path, hint);
                    }
                    return self.compile_struct_field_read(id, bound, path, hint);
                }
                let width = hint.unwrap_or_else(|| self.width_of(id));
                let bound_is_option_alias = matches!(root_ty, Ty::Option(_))
                    && matches!(self.types.expr_tys.get(&bound), Some(Ty::Option(_)));
                match self.compile_field_path_value(bound, path, &root_ty, width) {
                    Some(value) => Ok(value),
                    None => {
                        let msg = if bound_is_option_alias {
                            "cannot resolve this `?T` field (v0 restriction: a `?T`-typed \
                             local must be bound directly to `false` or a plain value of \
                             the wrapped type, not aliased from another `?T` value)"
                                .to_string()
                        } else if matches!(root_ty, Ty::Option(_)) {
                            format!("this `?T` value has no field `{}`", path.join("."))
                        } else {
                            "cannot resolve this struct field (v0 restriction: a struct-typed \
                             local must be bound directly to a struct literal, not aliased \
                             from another local)"
                                .to_string()
                        };
                        self.error(self.ast.expr_spans[root.0 as usize].clone(), msg);
                        Err(())
                    }
                }
            }
            _ => {
                self.error(
                    self.ast.expr_spans[root.0 as usize].clone(),
                    "unsupported struct reference in FIRRTL emission (v0 restriction)".to_string(),
                );
                Err(())
            }
        }
    }

    /// `x[i]` (single index), `x[hi..lo]` (slice), or `x[base +:
    /// width]`/`x[base -: width]` (indexed part-select) on a bits-typed
    /// (not memory) base. Each has a different rule for what may be
    /// dynamic (a runtime, not compile-time-constant, expression):
    /// - a slice's bounds must BOTH be static — types.rs already rejects
    ///   a non-const bound as a type error before this ever runs (a
    ///   slice's WIDTH can't be known otherwise), so the fallback error
    ///   below is unreachable in practice, kept only for defense in
    ///   depth;
    /// - a single index may be dynamic — always exactly 1 bit either
    ///   way, compiled via `dshr(x, i)` then taking bit 0 when `i` isn't
    ///   const, or the existing static `bits(x, i, i)` when it is;
    /// - indexed part-select's `base` may always be dynamic (that's the
    ///   whole point — a fixed-WIDTH slice starting somewhere runtime-
    ///   computed); only `width` must be static, already validated by
    ///   types.rs. Compiled the same way a dynamic index is (`dshr` to
    ///   bring the desired low bit down to position 0), then a STATIC
    ///   `bits(..., width-1, 0)` truncation — `width` being static is
    ///   exactly what makes this different from (and simpler than) a
    ///   fully dynamic slice, which has no such fixed truncation point.
    pub(crate) fn compile_bit_select(
        &mut self,
        id: ExprId,
        callee: ExprId,
        args: &[ExprId],
    ) -> Result<String, ()> {
        let base = self.compile_expr(callee)?;
        let Some(&arg) = args.first() else {
            self.error(
                self.ast.expr_spans[callee.0 as usize].clone(),
                "bit-select/slice takes exactly one argument".to_string(),
            );
            return Err(());
        };
        if let Expr::Binary { op, lhs, rhs } = self.ast.expr(arg).clone() {
            match op {
                BinOp::Range => {
                    // `const_eval` already accepts a bare `Int` or a
                    // sized literal (`8'd3`) equally — no new literal-
                    // recognition logic needed here, just reusing it
                    // instead of matching `Expr::Int`/`Expr::SizedInt`
                    // by hand.
                    let Some((hi, lo)) = self.const_eval(lhs).zip(self.const_eval(rhs)) else {
                        self.error(
                            self.ast.expr_spans[arg.0 as usize].clone(),
                            "a slice's bounds must be literal integers in FIRRTL \
                             emission (types.rs should already have rejected a \
                             non-const bound as a type error before this point)"
                                .to_string(),
                        );
                        return Err(());
                    };
                    if hi < lo {
                        self.error(
                            self.ast.expr_spans[arg.0 as usize].clone(),
                            format!(
                                "slice bounds must be high..low (got {hi}..{lo}); the \
                                 type checker accepts either order but FIRRTL's \
                                 `bits` primop needs hi >= lo"
                            ),
                        );
                        return Err(());
                    }
                    return Ok(format!("bits({base}, {hi}, {lo})"));
                }
                BinOp::PlusColon | BinOp::MinusColon => {
                    // Read the already-validated width off the bracket
                    // expression's own type (`id`, not `rhs`) instead of
                    // re-deriving it here with `const_eval`: types.rs's
                    // `const_eval` folds binary ops and `clog2`, but
                    // firrtl's own `const_eval` only recognizes a bare
                    // literal, so a width types.rs accepted as constant
                    // (e.g. `x[base +: clog2(8)]`) could const_eval to
                    // `None` right here — falling back to a wrong default
                    // width would silently emit a too-narrow select
                    // instead of erroring or using the real width.
                    let width = self.width_of(id);
                    let ow = self.width_of(lhs);
                    let off = self.compile_expr_hinted(lhs, Some(ow))?;
                    let shift_by = match op {
                        // `+:`: the low bit of the result is `base`
                        // itself — shift it straight down to position 0.
                        BinOp::PlusColon => off,
                        // `-:`: the low bit of the result is
                        // `base - (width - 1)` — shift THAT down to
                        // position 0 instead, same `tail(sub(...), 1)`
                        // carry-bit truncation any other subtraction in
                        // this emitter uses.
                        BinOp::MinusColon => format!(
                            "tail(sub({off}, UInt<{ow}>({})), 1)",
                            width.saturating_sub(1)
                        ),
                        _ => unreachable!(),
                    };
                    return Ok(format!("bits(dshr({base}, {shift_by}), {}, 0)", width - 1));
                }
                _ => {}
            }
        }
        // A single index — static (the existing fast path, unchanged)
        // or dynamic (new: shift the target bit down to position 0,
        // then take it — always exactly 1 bit regardless).
        if let Some(i) = self.const_eval(arg) {
            return Ok(format!("bits({base}, {i}, {i})"));
        }
        let iw = self.width_of(arg);
        let i = self.compile_expr_hinted(arg, Some(iw))?;
        Ok(format!("bits(dshr({base}, {i}), 0, 0)"))
    }

    pub(crate) fn compile_unop(
        &mut self,
        id: ExprId,
        op: UnOp,
        operand: ExprId,
    ) -> Result<String, ()> {
        match op {
            UnOp::Neg => {
                let w = self.width_of(id);
                let e = self.compile_expr_hinted(operand, Some(w))?;
                Ok(format!("tail(sub(UInt<{w}>(0), {e}), 1)"))
            }
            // `not` and `~` emit the IDENTICAL FIRRTL `not` primop — types.rs
            // already made them a real, distinct operator, not pure
            // aliasing at this layer's expense: `not`'s own typing rule
            // requires a `bits[1]` operand (a guardrail against
            // accidentally bitwise-negating a wider value), while `~`
            // accepts any width. Bitwise-complementing a single bit IS
            // logical negation, so once that's enforced, there is nothing
            // left for emission to do differently.
            UnOp::BitNot | UnOp::Not => {
                let w = self.width_of(id);
                let e = self.compile_expr_hinted(operand, Some(w))?;
                Ok(format!("not({e})"))
            }
        }
    }

    /// `id`'s own `Bits` width, resolved through `self.locals`'s
    /// substitution chain when the ordinary static lookup
    /// (`types.expr_tys`) comes up empty — the case for any expression
    /// reachable only from a GENERIC callee's own body. `types/mod.rs`'s
    /// own module doc comment: "Generic bodies (widths depending on
    /// unsolved implicit params) are shape-checked only; their widths
    /// check numerically at each concrete call site after instantiation"
    /// — this IS that per-call-site numeric check, just not limited to
    /// "the return value" or "a builtin's own argument" the way the
    /// callee-inlining machinery's OTHER width lookups are: a bare
    /// reference to a generic param (or a `let` aliasing one, however
    /// many hops deep) is substituted via `self.locals` — the SAME map
    /// `compile_expr_hinted`'s own `Ident` arm consults to compile a
    /// VALUE — recursing until it bottoms out at a REAL, concretely-
    /// typed expression at the call site. An arithmetic/bitwise/shift
    /// node built from generic operands (`x + y`, `x << 1`, `0 - x`)
    /// combines their OWN resolved widths via `combine_bits_width`
    /// (`types/mod.rs`) — the SAME rule `type_binop` itself uses, not a
    /// second, independently-maintained copy of it — handling one-side-
    /// literal absorption (`x + 1`) identically to `type_binop`'s own
    /// `(Bits, Int)`/`(Int, Bits)` arms. `None` on anything this doesn't
    /// recognize (a mem read, a call, a struct field, ...) — those
    /// either already have a concrete width from the fast path above, or
    /// genuinely have none to resolve, same as before this existed.
    pub(crate) fn resolve_bits_width(&self, id: ExprId) -> Option<u64> {
        if let Some(Ty::Bits(Width::Known(w))) = self.types.expr_tys.get(&id) {
            return Some(*w);
        }
        match self.ast.expr(id) {
            Expr::Ident(_) => {
                let def = self.res.expr_defs.get(&id)?;
                let sub = *self.locals.get(def)?;
                self.resolve_bits_width(sub)
            }
            Expr::Binary { op, lhs, rhs }
                if matches!(
                    op,
                    BinOp::Add
                        | BinOp::Sub
                        | BinOp::Mul
                        | BinOp::Div
                        | BinOp::Rem
                        | BinOp::Shl
                        | BinOp::Shr
                        | BinOp::AShr
                        | BinOp::BitAnd
                        | BinOp::BitOr
                        | BinOp::BitXor
                ) =>
            {
                if matches!(self.ast.expr(*lhs), Expr::Int(_)) {
                    return self.resolve_bits_width(*rhs);
                }
                if matches!(self.ast.expr(*rhs), Expr::Int(_)) {
                    return self.resolve_bits_width(*lhs);
                }
                let a = self.resolve_bits_width(*lhs)?;
                let b = self.resolve_bits_width(*rhs)?;
                match crate::types::combine_bits_width(*op, Width::Known(a), Width::Known(b)) {
                    Width::Known(w) => Some(w),
                    Width::Unknown => None,
                }
            }
            // The two synthesizable builtins with a REAL, computable
            // result-width rule (`types/expr.rs`'s own `type_builtin_
            // call` -- `prio_result_width`, the shared source of truth,
            // for `prio`; a plain sum, too small a rule to be worth
            // sharing, for `pack`) -- so a generic body's `let g =
            // prio(reqs); let g2 = g + 1; return g2` resolves too, not
            // just a builtin's own DIRECT return. `trunc`'s one-arg form
            // has no such rule at all (its own doc comment: "width
            // INFERRED from wherever this call's own result is used",
            // hint-only by design) and stays `None`, deliberately. A
            // nested call to another user-defined generic function falls
            // to `resolve_nested_call_width`, below.
            Expr::Call { callee, args } => {
                let def = self.res.expr_defs.get(callee)?;
                match self.res.def(*def).name.as_str() {
                    "prio" => {
                        let arg_w = self.resolve_bits_width(*args.first()?)?;
                        Some(crate::types::prio_result_width(arg_w))
                    }
                    "pack" => {
                        let mut total = 0u64;
                        for &a in args {
                            total += self.resolve_bits_width(a)?;
                        }
                        Some(total)
                    }
                    // Same rationale as `prio`/`pack` just above: a real,
                    // computable result-width rule (`popcount_result_
                    // width`, or the argument's own width unchanged for
                    // the rest), so these resolve inside a generic body
                    // too, not just at their own direct return.
                    "popcount" => {
                        let arg_w = self.resolve_bits_width(*args.first()?)?;
                        Some(crate::types::popcount_result_width(arg_w))
                    }
                    "reverse" | "rotl" | "rotr" => self.resolve_bits_width(*args.first()?),
                    "mux" => {
                        let a = self.resolve_bits_width(*args.get(1)?)?;
                        let b = self.resolve_bits_width(*args.get(2)?)?;
                        Some(a.max(b))
                    }
                    // `max`'s own result width is the MAX of whichever
                    // arguments resolve to a real `Bits` width; `min`'s is
                    // the MIN (see `types.rs`'s own `"max"|"min"` arm doc
                    // comment on why the two directions differ — `min`
                    // can never exceed its narrowest operand). Both
                    // deliberately `filter_map`, not `?`-propagated like
                    // every arm above: a bare `Int` argument (`max(a,
                    // 10)`) contributes no width of its own (it's a
                    // literal that absorbs into the others, same as
                    // `type_binop`'s `(Bits, Int)` rule), not a resolution
                    // FAILURE the way an unresolvable `Bits` operand would
                    // be. If NONE resolve, this call is the deliberately
                    // compile-time-only all-`Int` shape (`types.rs` typed
                    // it `Ty::Int`), which has no `Bits` width at all --
                    // `None` here is the correct answer, not a gap.
                    "max" => args
                        .iter()
                        .filter_map(|&a| self.resolve_bits_width(a))
                        .max(),
                    "min" => args
                        .iter()
                        .filter_map(|&a| self.resolve_bits_width(a))
                        .min(),
                    _ => match self.res.def(*def).kind {
                        DefKind::Fn | DefKind::Impl => self.resolve_nested_call_width(*def, args),
                        _ => None,
                    },
                }
            }
            _ => None,
        }
    }

    /// A nested call to ANOTHER user-defined generic function --
    /// `Inner(x)` combined with further arithmetic inside `Outer`'s own
    /// generic body -- resolves the same two-step way `type_call`
    /// (types/expr.rs) itself instantiates a call at type-check time:
    /// build an `env` mapping `Inner`'s own implicit width params to
    /// concrete widths (here, by resolving THIS call's own arguments
    /// through `resolve_bits_width` again, recursively, rather than
    /// reading them off already-known static types), then evaluate
    /// `Inner`'s declared return type's width expression against that
    /// `env` (`const_eval_expr` -- the SAME evaluator `type_call`'s own
    /// `eval_ty` uses under the hood, not a second copy). A param typed
    /// something other than a bare `bits[N]` (fixed-width, or a
    /// non-`Bits` type entirely) simply contributes nothing to `env` --
    /// mirroring `type_call`'s own `if let Some(pdef) = ...` guard --
    /// and a return type that isn't `bits[...]`-shaped, or whose width
    /// expression needs an entry `env` doesn't have, falls out to `None`
    /// the same way every other unresolvable shape here does. Two params
    /// declared to share one implicit name (`x : [n], y : [n]`) whose
    /// arguments resolve to genuinely DIFFERENT concrete widths also
    /// bails to `None` -- mirroring `type_call`'s own conflict check,
    /// and load-bearing here in a way it merely duplicates there: unlike
    /// a top-level concrete call site (where `check_assignable` rejects
    /// the mismatch outright), a nested call inside another generic body
    /// never reaches that check (both args are `Width::Unknown` during
    /// the enclosing body's own single generic type-check pass, so the
    /// comparison is vacuous), and `compile_call`'s own real inlining
    /// substitutes each param independently with no cross-param
    /// consistency check of its own -- so a WRONG, order-dependent
    /// guess here (`env`'s `HashMap::insert` silently keeping whichever
    /// width was written last) would size a literal or feed `width_of`
    /// a number that disagrees with what the real inlined value actually
    /// computes to, exactly the miscompile class the rest of this
    /// resolver exists to avoid. `None` here just means "this call's own
    /// width genuinely isn't well-defined," the same honest answer as
    /// every other unresolvable shape.
    ///
    /// Never recurses into `Inner`'s own BODY, only its declared
    /// signature (params + return-type expression) -- so unlike
    /// `compile_call`'s own inlining, a mutually-recursive callee pair
    /// can't blow the stack here even transiently; `find_call_cycle`
    /// (calls.rs) still separately rejects such a pair before either
    /// side is ever inlined for real.
    fn resolve_nested_call_width(&self, def: DefId, args: &[ExprId]) -> Option<u64> {
        let item = self
            .res
            .item_defs
            .iter()
            .find(|(_, d)| **d == def)
            .map(|(item, _)| *item)?;
        let Item::Fn { params, ret, .. } = self.ast.item(item) else {
            return None;
        };
        if params.len() != args.len() {
            return None;
        }
        let mut env: HashMap<DefId, u64> = HashMap::new();
        for (param, arg) in params.iter().zip(args) {
            if let Some(pdef) = crate::types::implicit_width_param(self.ast, self.res, param.ty) {
                let w = self.resolve_bits_width(*arg)?;
                if let Some(prev) = env.insert(pdef, w)
                    && prev != w
                {
                    return None;
                }
            }
        }
        let width_expr = crate::types::bits_width_expr(self.ast, self.res, (*ret)?)?;
        crate::types::const_eval_expr(self.ast, self.res, width_expr, &env)
    }

    /// A literal operand has no width of its own; it absorbs one from
    /// its sibling. Arithmetic already has the absorbed width recorded
    /// on the whole expression (`types.expr_tys[id]`, or — inside a
    /// generic callee body — `resolve_bits_width`'s own substitution-
    /// aware fallback); comparisons don't (their own type is always
    /// `bits[1]`), so fall back to whichever side is a concrete,
    /// non-literal type.
    pub(crate) fn compile_binop(
        &mut self,
        id: ExprId,
        op: BinOp,
        lhs: ExprId,
        rhs: ExprId,
    ) -> Result<String, ()> {
        if matches!(op, BinOp::Shl | BinOp::Shr | BinOp::AShr) {
            return self.compile_shift(op, lhs, rhs);
        }
        // For every op below, one side being a bare literal (`Ty::Int`)
        // means the checker typed the whole expression as the *other*
        // side's own width (types.rs's mixed-operand rule), so hinting
        // the literal to `resolve_bits_width(id)` always lands on the
        // right value — whether or not this op is one whose "both sides
        // bits" rule also happens to equal that width (it does for
        // every op here except Mul, handled below).
        let hint = if matches!(
            op,
            BinOp::Add
                | BinOp::Sub
                | BinOp::Mul
                | BinOp::Div
                | BinOp::Rem
                | BinOp::BitAnd
                | BinOp::BitOr
                | BinOp::BitXor
        ) {
            self.resolve_bits_width(id)
        } else if matches!(self.ast.expr(lhs), Expr::Int(_)) {
            self.resolve_bits_width(rhs)
        } else if matches!(self.ast.expr(rhs), Expr::Int(_)) {
            self.resolve_bits_width(lhs)
        } else {
            None
        };
        let l = self.compile_expr_hinted(lhs, hint)?;
        let r = self.compile_expr_hinted(rhs, hint)?;
        Ok(match op {
            BinOp::Add => format!("tail(add({l}, {r}), 1)"),
            BinOp::Sub => format!("tail(sub({l}, {r}), 1)"),
            BinOp::Mul => {
                // `mul` sums both compiled operand widths. When neither
                // side is a literal that already equals the checker's
                // target width (types.rs sums too, for a genuine
                // bits*bits multiply); a literal absorbed `hint` above
                // instead, so `mul` overshoots by exactly that width —
                // trim back down like `add`/`sub` do for their carry bit.
                let wl = if matches!(self.ast.expr(lhs), Expr::Int(_)) {
                    hint
                } else {
                    self.resolve_bits_width(lhs)
                }
                .unwrap_or(1);
                let wr = if matches!(self.ast.expr(rhs), Expr::Int(_)) {
                    hint
                } else {
                    self.resolve_bits_width(rhs)
                }
                .unwrap_or(1);
                let target = self.resolve_bits_width(id).unwrap_or(wl + wr);
                match (wl + wr).checked_sub(target) {
                    Some(drop) if drop > 0 => format!("tail(mul({l}, {r}), {drop})"),
                    _ => format!("mul({l}, {r})"),
                }
            }
            // FIRRTL's own `div`/`rem` primops don't width like `mul`'s
            // clean "sum, then trim the excess" rule — confirmed against
            // real firtool (a `node`, not an explicitly-widthed output, to
            // see the primop's OWN inferred width rather than a `connect`'s
            // silent truncate/extend): `div(a, b)` is exactly `w(a)` (the
            // DIVIDEND's width, UInt semantics — never `max`/sum), `rem(a,
            // b)` is `min(w(a), w(b))`. Both are always <= the checker's
            // own target width (`max(w(a), w(b))`, the same mixed-operand
            // rule every other op here shares), so only ever a `pad` UP is
            // needed, never a `tail` trim down like `add`/`sub`/`mul`.
            BinOp::Div => {
                let wl = if matches!(self.ast.expr(lhs), Expr::Int(_)) {
                    hint
                } else {
                    self.resolve_bits_width(lhs)
                }
                .unwrap_or(1);
                let target = self.resolve_bits_width(id).unwrap_or(wl);
                match target.checked_sub(wl) {
                    Some(pad) if pad > 0 => format!("pad(div({l}, {r}), {target})"),
                    _ => format!("div({l}, {r})"),
                }
            }
            BinOp::Rem => {
                let wl = if matches!(self.ast.expr(lhs), Expr::Int(_)) {
                    hint
                } else {
                    self.resolve_bits_width(lhs)
                }
                .unwrap_or(1);
                let wr = if matches!(self.ast.expr(rhs), Expr::Int(_)) {
                    hint
                } else {
                    self.resolve_bits_width(rhs)
                }
                .unwrap_or(1);
                let firrtl_w = wl.min(wr);
                let target = self.resolve_bits_width(id).unwrap_or(firrtl_w);
                match target.checked_sub(firrtl_w) {
                    Some(pad) if pad > 0 => format!("pad(rem({l}, {r}), {target})"),
                    _ => format!("rem({l}, {r})"),
                }
            }
            BinOp::BitAnd => format!("and({l}, {r})"),
            BinOp::BitOr => format!("or({l}, {r})"),
            BinOp::BitXor => format!("xor({l}, {r})"),
            BinOp::Eq => format!("eq({l}, {r})"),
            BinOp::Ne => format!("neq({l}, {r})"),
            BinOp::Lt => format!("lt({l}, {r})"),
            BinOp::Le => format!("leq({l}, {r})"),
            BinOp::Gt => format!("gt({l}, {r})"),
            BinOp::Ge => format!("geq({l}, {r})"),
            _ => {
                self.error(
                    self.ast.expr_spans[id.0 as usize].clone(),
                    "this operator is not yet supported in FIRRTL emission (v0 \
                     restriction)"
                        .to_string(),
                );
                return Err(());
            }
        })
    }

    /// Both static (literal-amount) and dynamic (runtime-amount) shifts,
    /// covering `<<`/`>>`/`>>>`. types.rs keeps the LEFT operand's width
    /// for all three either way (matching Verilog's fixed-width shift
    /// semantics, not FIRRTL's own `shl`/`dshl`/`shr`/`dshr`, which each
    /// grow or shrink) — so every case below ends by bringing the FIRRTL
    /// primop's own result back to `w`, the same shape whether the
    /// amount is known at compile time or not.
    ///
    /// Static: `shl` grows the width BY the (literal) shift amount,
    /// `shr`/`ashr` shrink it by that same amount — brought back to `w`
    /// by dropping the high bits that fell off (`shl`) or padding back
    /// up (`shr`/`ashr`).
    ///
    /// Dynamic: FIRRTL's `dshl(a, b)`/`dshr(a, b)` widths were confirmed
    /// against real firtool, not assumed from the spec text (a `node`,
    /// not a `connect` into an explicitly-widthed output, to see the
    /// primop's own inferred width): `dshl` grows to `w(a) + 2^w(b) - 1`
    /// — the exponential term is `b`'s own WIDTH determining the largest
    /// shift amount it could possibly hold, a STATIC quantity even
    /// though `b`'s VALUE is dynamic, so the amount to `tail` back down
    /// to `w` (`2^w(b) - 1`) is still a compile-time constant, computed
    /// the identical way the static case's constant drop amount is.
    /// `dshr`, unlike static `shr`, does NOT shrink at all — it's
    /// already exactly `w(a)`, so no `pad`/`tail` is needed there, only
    /// for `dshl`. A wide shift-amount operand (`b`) produces a
    /// correspondingly wide (if wasteful) `dshl` intermediate before the
    /// `tail` trims it back down — not restricted here, the same
    /// "compile what's asked" stance the rest of this emitter takes
    /// toward hardware size.
    ///
    /// `>>>` (`AShr`): this language has no signed type (TODO.md), so
    /// arithmetic shift is a per-operator choice, not a property of the
    /// operand's own type — the sign-extending behavior comes entirely
    /// from wrapping the shift in `asSInt`/`asUInt` at emission time.
    /// Confirmed against real firtool + a real simulation, not assumed
    /// from spec text: `asUInt(pad(shr(asSInt(l), n), w))` for the
    /// static case (the inner `shr` shrinks exactly like unsigned `shr`
    /// does, and `pad` on an `SInt` sign-extends, unlike `pad` on a
    /// `UInt`, which is why the cast has to happen before the pad, not
    /// after); `asUInt(dshr(asSInt(l), r))` for the dynamic case, with
    /// no pad needed, for the identical reason unsigned `dshr` needs
    /// none — width already stays `w(a)` regardless of sign.
    pub(crate) fn compile_shift(
        &mut self,
        op: BinOp,
        lhs: ExprId,
        rhs: ExprId,
    ) -> Result<String, ()> {
        let w = self.width_of(lhs);
        let l = self.compile_expr_hinted(lhs, Some(w))?;
        if let Some(n) = self.const_eval(rhs) {
            return Ok(match op {
                BinOp::Shl => format!("tail(shl({l}, {n}), {n})"),
                BinOp::Shr => format!("pad(shr({l}, {n}), {w})"),
                BinOp::AShr => format!("asUInt(pad(shr(asSInt({l}), {n}), {w}))"),
                _ => unreachable!("compile_shift only called for Shl/Shr/AShr"),
            });
        }
        let rw = self.width_of(rhs);
        let r = self.compile_expr_hinted(rhs, Some(rw))?;
        Ok(match op {
            BinOp::Shl => {
                let grown = (1u64 << rw) - 1;
                format!("tail(dshl({l}, {r}), {grown})")
            }
            BinOp::Shr => format!("dshr({l}, {r})"),
            BinOp::AShr => format!("asUInt(dshr(asSInt({l}), {r}))"),
            _ => unreachable!("compile_shift only called for Shl/Shr/AShr"),
        })
    }
}

/// Masks a literal's raw value down to `w` bits before it's embedded in
/// FIRRTL's `UInt<w>(v)` literal syntax — a no-op when `v` already fits.
/// Needed as of `.!`'s generalization to `check_literal_fits`/`Expr::
/// SizedInt`'s own too-wide check (types.rs): unlike a `connect` between
/// two differently-sized real signals (which FIRRTL/firtool truncates
/// implicitly, confirmed empirically), the LITERAL syntax itself demands
/// `v` fit `w` exactly — firtool hard-errors ("initializer too wide for
/// declared width") on an out-of-range literal even where the
/// surrounding `connect` would have truncated a real signal for free.
/// `.!` bypassing the type-checker's own complaint doesn't change that
/// FIRRTL-level constraint, so emission has to do the truncation `.!`
/// asked for itself, on the literal's own value, before it ever reaches
/// firtool.
pub(crate) fn mask_to_width(v: u64, w: u64) -> u64 {
    match 1u64.checked_shl(w as u32) {
        Some(cap) => v % cap,
        None => v,
    }
}
