//! Expression-level type checking: `type_expr`'s per-`Expr`-variant
//! dispatch (`type_expr_inner`, populating `expr_tys` for every
//! expression it types), solver-2's width-combining rules for binary
//! operators (`type_binop`), the `Stmt::IfLet`/`check_cond` helpers that
//! recognize a bare fallible shape (`is_failing_call`, `is_fifo_deq`),
//! bracket dispatch — memory read, bit/slice select, fifo op, or a
//! bracketed builtin (`type_bracket`) — and solver-1's call
//! instantiation (`type_call`, `type_builtin_call`).

use super::{OPTION_FIELDS, Ty, TypeChecker, Width, bits_needed};
use crate::ast::{BinOp, Expr, ExprId, Item, UnOp};
use crate::resolve::{DefId, DefKind};
use std::collections::HashMap;

impl<'a> TypeChecker<'a> {
    pub(crate) fn type_expr(&mut self, id: ExprId, locals: &mut HashMap<DefId, Ty>) -> Ty {
        let ty = self.type_expr_inner(id, locals);
        self.types.expr_tys.insert(id, ty.clone());
        ty
    }

    /// `type_expr`'s hint-aware sibling, used ONLY by `Stmt::Assign`'s
    /// own LHS-declared-type lookup and `Stmt::Return`'s own `ret` --
    /// the two positions with a genuinely known expected type available
    /// BEFORE the expression itself is typed. Narrowly scoped to the
    /// single shape that benefits: `id` itself being a DIRECT call to a
    /// user `fn`/`impl`/`spec`, letting `type_call`'s own solve use the
    /// caller's expected type (see that fn's own doc comment). Anything
    /// else (a nested call two levels down, any other expr shape at
    /// all) falls through to the ordinary hint-less `type_expr`
    /// unchanged -- this is deliberately NOT general top-down hint
    /// propagation through the whole recursive descent, matching this
    /// pass's own "opt-in, not blanket" convention everywhere else.
    pub(crate) fn type_expr_with_hint(
        &mut self,
        id: ExprId,
        locals: &mut HashMap<DefId, Ty>,
        hint: Option<&Ty>,
    ) -> Ty {
        if let Expr::Call { callee, args } = self.ast.expr(id).clone()
            && let Some(def) = self.res.expr_defs.get(&callee).copied()
            && matches!(
                self.res.def(def).kind,
                DefKind::Fn | DefKind::Impl | DefKind::Spec
            )
        {
            let ty = self.type_call(id, callee, &args, locals, hint);
            self.types.expr_tys.insert(id, ty.clone());
            return ty;
        }
        self.type_expr(id, locals)
    }

    /// Whether `id` is EXACTLY a call to the builtin `trunc` with a
    /// single argument -- `type_call`'s own backward-fill step (see its
    /// doc comment) only ever recognizes this one shape, not any other
    /// `Width::Unknown`-producing expression.
    fn is_one_arg_trunc_call(&self, id: ExprId) -> bool {
        let Expr::Call { callee, args } = self.ast.expr(id) else {
            return false;
        };
        args.len() == 1
            && self.res.expr_defs.get(callee).is_some_and(|&d| {
                let def = self.res.def(d);
                def.kind == DefKind::Builtin && def.name == "trunc"
            })
    }

    fn type_expr_inner(&mut self, id: ExprId, locals: &mut HashMap<DefId, Ty>) -> Ty {
        match self.ast.expr(id).clone() {
            Expr::Int(_) => Ty::Int,
            // Unlike a bare `Int`, a sized literal has its own definite
            // width, so it types directly as `Bits(Known(width))` — no
            // "absorb from context" — and is range-checked right here,
            // against ITS OWN declared width, rather than deferred to
            // `check_literal_fits` at whatever coercion site it's later
            // used in (matches the same `bits_needed` helper that uses).
            // Self-gated on `.!` the same way `check_literal_fits` is
            // (`ast.lossy`, keyed on `id` itself — `8'd300.!` marks THIS
            // node directly, unlike the mid-operator spelling which has
            // no bearing on a lone literal): FIRRTL emission
            // (`firrtl/expr.rs`'s `mask_to_width`) truncates the value to
            // fit `width` regardless, so a lossy-marked oversized sized
            // literal compiles to its low `width` bits, same as any
            // other `.!`-silenced truncation.
            Expr::SizedInt { width, value } => {
                if bits_needed(value) > width && !self.ast.lossy.contains(&id) {
                    self.error(
                        self.expr_span(id),
                        format!("{value} does not fit in [{width}]"),
                    );
                }
                Ty::Bits(Width::Known(width))
            }
            Expr::Wildcard => Ty::Unknown,
            Expr::Ident(_) => {
                let Some(def) = self.res.expr_defs.get(&id).copied() else {
                    return Ty::Unknown;
                };
                // Bare state idents carry their state type; indexing and
                // fifo ops peel Mem/Fifo wrappers at the use site.
                if let Some(state) = self.state_tys.get(&def) {
                    return state.clone();
                }
                if let Some(local) = locals.get(&def) {
                    return local.clone();
                }
                match self.res.def(def).kind {
                    DefKind::ImplicitParam => Ty::Int,
                    DefKind::Inst => {
                        self.error(
                            self.expr_span(id),
                            format!(
                                "cannot use instance `{}` as a value; access one of its \
                                 ports (`{}.port`)",
                                self.res.def(def).name,
                                self.res.def(def).name
                            ),
                        );
                        Ty::Unknown
                    }
                    _ => Ty::Unknown,
                }
            }
            Expr::Unary { op, operand } => {
                let t = self.type_expr(operand, locals);
                match t {
                    Ty::Bits(_) | Ty::Int | Ty::Unknown => {}
                    other => {
                        self.error(
                            self.expr_span(id),
                            format!("unary operator needs bits, got {other}"),
                        );
                        return Ty::Unknown;
                    }
                }
                // `not` is a real, distinct operator from `~`, not pure
                // sugar for it: both compile to the identical FIRRTL
                // `not` primop (see firrtl/expr.rs), but `not` additionally
                // requires its operand already be `bits[1]` — a
                // guardrail against accidentally bitwise-negating a
                // wider value (`not x` on a `bits[8]` almost certainly
                // means "did you mean a comparison, or `~`?", not "flip
                // every bit"), since `check_cond` (stmt.rs) already requires
                // every condition position to be exactly `bits[1]`
                // anyway — there is no implicit "nonzero is true"
                // coercion anywhere in this language for `not` to
                // usefully mean something wider.
                if op == UnOp::Not
                    && !matches!(
                        t,
                        Ty::Bits(Width::Known(1))
                            | Ty::Bits(Width::Unknown)
                            | Ty::Unknown
                            | Ty::Int
                    )
                {
                    self.error(
                        self.expr_span(id),
                        format!(
                            "`not` needs a [1] operand, got {t}; use `~` for a \
                             bitwise complement of a wider value, or compare \
                             explicitly"
                        ),
                    );
                    return Ty::Unknown;
                }
                t
            }
            Expr::Binary { op, lhs, rhs } => {
                let l = self.type_expr(lhs, locals);
                let r = self.type_expr(rhs, locals);
                self.type_binop(op, l, r, lhs, rhs, id)
            }
            // `(cond)?` ordinarily just passes `inner`'s own type
            // through (its "must be bits[1]" side is `check_cond`'s
            // job, not this fn's). `opt?`, `opt : ?T`, is different: the
            // WHOLE point is unwrapping, so the guard's own type becomes
            // `T`, not `?T` — generalizing `?` from "fails unless
            // bits[1]-true" to "fails unless present, yielding T",
            // reusing the exact same `fails`-folding machinery `f.Deq[]`
            // already has (see `effects.rs`'s `Expr::Guard` handling).
            Expr::Guard(inner) => match self.type_expr(inner, locals) {
                Ty::Option(t) => *t,
                other => other,
            },
            Expr::Field { base, name } => {
                if let Some(module_def) = self.instance_module_of(base) {
                    self.types.expr_tys.insert(base, Ty::Unknown);
                    match self.find_port(module_def, &name) {
                        Some((DefKind::Output, port_ty)) => port_ty,
                        Some((DefKind::Io, _)) => {
                            self.error(
                                self.expr_span(id),
                                format!(
                                    "cannot read `{name}`: it is an io port on this instance \
                                     (io ports carry no value — the only legal use is \
                                     `attach`ing it to another io port)"
                                ),
                            );
                            Ty::Unknown
                        }
                        Some((_, _)) => {
                            self.error(
                                self.expr_span(id),
                                format!(
                                    "cannot read `{name}`: it is an input port on this \
                                     instance (only output ports can be read)"
                                ),
                            );
                            Ty::Unknown
                        }
                        None => {
                            self.error(
                                self.expr_span(id),
                                format!("this instance has no port `{name}`"),
                            );
                            Ty::Unknown
                        }
                    }
                } else {
                    match self.type_expr(base, locals) {
                        Ty::Handle(inner) => match name.as_str() {
                            "result" => *inner,
                            "done" => Ty::Bits(Width::Known(1)),
                            _ => {
                                self.error(
                                    self.expr_span(id),
                                    format!(
                                        "a handle has no field `{name}`; only `.result` and \
                                         `.done` are readable"
                                    ),
                                );
                                Ty::Unknown
                            }
                        },
                        Ty::Struct { def, name: sname } => {
                            let fields = self.types.struct_fields.get(&def).cloned();
                            match fields
                                .as_ref()
                                .and_then(|fs| fs.iter().find(|(fname, _)| fname == &name))
                            {
                                Some((_, fty)) => fty.clone(),
                                None => {
                                    self.error(
                                        self.expr_span(id),
                                        format!("struct `{sname}` has no field `{name}`"),
                                    );
                                    Ty::Unknown
                                }
                            }
                        }
                        // `?T`'s two synthetic fields, same escape hatch
                        // a `spawn` handle's `.result`/`.done` already
                        // has: `.valid`/`.data` read WITHOUT unwrap-or-
                        // fail (`opt?`'s job) — the non-failing
                        // alternative `if opt.valid { ...opt.data... }
                        // else { ... }` gives.
                        Ty::Option(inner) => {
                            if !OPTION_FIELDS.contains(&name.as_str()) {
                                self.error(
                                    self.expr_span(id),
                                    format!(
                                        "a `?T` value only has `.valid`/`.data` fields, \
                                         not `.{name}`"
                                    ),
                                );
                                Ty::Unknown
                            } else if name == "valid" {
                                Ty::Bits(Width::Known(1))
                            } else {
                                *inner
                            }
                        }
                        Ty::Unknown => Ty::Unknown,
                        other => {
                            self.error(
                                self.expr_span(id),
                                format!("cannot access field `{name}` on {other}"),
                            );
                            Ty::Unknown
                        }
                    }
                }
            }
            Expr::Spawn(inner) => {
                let ret = self.type_expr(inner, locals);
                Ty::Handle(Box::new(ret))
            }
            Expr::Bracket { callee, args } => self.type_bracket(id, callee, &args, locals),
            Expr::Call { callee, args } => self.type_call(id, callee, &args, locals, None),
            Expr::ListLit(items) => {
                let Some((&first, rest)) = items.split_first() else {
                    self.error(
                        self.expr_span(id),
                        "a list literal cannot be empty (its element type would be \
                         unknowable)"
                            .to_string(),
                    );
                    return Ty::Unknown;
                };
                let elem = self.type_expr(first, locals);
                for &item in rest {
                    let t = self.type_expr(item, locals);
                    self.check_assignable(&t, &elem, item, "list element");
                }
                Ty::List(Box::new(elem))
            }
            // Only meaningful as a list-slice `Bracket` argument
            // (`type_bracket` re-matches the raw AST shape there for the
            // real element-type/slice rule); reached here only via the
            // generic per-subexpression walk `type_bracket` already does
            // first, or if used somewhere illegal — `Unknown`, the same
            // treatment the existing two-sided `BinOp::Range` gets in
            // `type_binop` when it shows up outside a bracket.
            Expr::Range { lo, hi } => {
                if let Some(lo) = lo {
                    self.type_expr(lo, locals);
                }
                if let Some(hi) = hi {
                    self.type_expr(hi, locals);
                }
                Ty::Unknown
            }
            // `A or B or C` types like `ListLit`'s element unification:
            // every alternative (a fifo op's element type, or a plain
            // default value) must agree with the first's type. Shape
            // rules (which alts must be fifo ops) are firrtl/checks.rs's
            // job, same division as everywhere else in this module.
            Expr::Or(alts) => {
                let Some((&first, rest)) = alts.split_first() else {
                    self.error(
                        self.expr_span(id),
                        "`or` needs at least two alternatives, e.g. `a.Deq[] or b.Deq[]`"
                            .to_string(),
                    );
                    return Ty::Unknown;
                };
                let elem = self.type_expr(first, locals);
                for &alt in rest {
                    let t = self.type_expr(alt, locals);
                    self.check_assignable(&t, &elem, alt, "`or` alternative");
                }
                elem
            }
            // `Name { field: expr, ..., ..base }` — v0 requires either an
            // EXHAUSTIVE, one-shot field list (matching Rust's own
            // struct-literal rule: no defaults for a field this literal
            // doesn't mention) or a trailing `..base` supplying every
            // field this literal DOESN'T name — never both partially:
            // `base` fills whatever's absent from THIS literal's own
            // list, it does not recurse into a nested struct/Option
            // field that's itself only partially given. A duplicate
            // field is almost certainly a typo, not a deliberate "last
            // one wins" overwrite. A bad struct NAME (unresolved, or
            // resolved to something that isn't `DefKind::Struct`) is
            // already reported by resolve.rs — this stays defensive
            // (`Ty::Unknown`, no second error) rather than re-checking
            // the same thing.
            Expr::StructLit { name, fields, base } => {
                let Some(&struct_def) = self.res.expr_defs.get(&name) else {
                    for (_, value) in &fields {
                        self.type_expr(*value, locals);
                    }
                    if let Some(base) = base {
                        self.type_expr(base, locals);
                    }
                    return Ty::Unknown;
                };
                let struct_name = match self.ast.expr(name) {
                    Expr::Ident(n) => n.clone(),
                    _ => String::new(),
                };
                let Some(declared) = self.types.struct_fields.get(&struct_def).cloned() else {
                    for (_, value) in &fields {
                        self.type_expr(*value, locals);
                    }
                    if let Some(base) = base {
                        self.type_expr(base, locals);
                    }
                    return Ty::Unknown;
                };
                let mut seen: HashMap<String, ExprId> = HashMap::new();
                for (fname, value) in &fields {
                    let vty = self.type_expr(*value, locals);
                    if seen.contains_key(fname) {
                        self.error(
                            self.expr_span(*value),
                            format!("field `{fname}` is given more than once"),
                        );
                        continue;
                    }
                    seen.insert(fname.clone(), *value);
                    match declared.iter().find(|(dname, _)| dname == fname) {
                        Some((_, dty)) => {
                            self.check_assignable(&vty, dty, *value, "struct field");
                            self.check_literal_fits(*value, dty);
                        }
                        None => {
                            self.error(
                                self.expr_span(*value),
                                format!("struct `{struct_name}` has no field `{fname}`"),
                            );
                        }
                    }
                }
                let result = Ty::Struct {
                    def: struct_def,
                    name: struct_name.clone(),
                };
                match base {
                    Some(base) => {
                        let base_ty = self.type_expr(base, locals);
                        self.check_assignable(&base_ty, &result, base, "`..` base");
                    }
                    None => {
                        let missing: Vec<&str> = declared
                            .iter()
                            .map(|(dname, _)| dname.as_str())
                            .filter(|dname| !seen.contains_key(*dname))
                            .collect();
                        if !missing.is_empty() {
                            self.error(
                                self.expr_span(id),
                                format!(
                                    "struct `{struct_name}` literal is missing field(s): {}; \
                                     use `..base` to fill in the rest, or give each one \
                                     explicitly",
                                    missing.join(", ")
                                ),
                            );
                        }
                    }
                }
                result
            }
            // `?T` has no meaning as a VALUE expression, only a type
            // (`eval_ty`'s own `Expr::OptionTy` arm handles it there) —
            // reachable here only if `?T` shows up somewhere that isn't
            // actually a type position (e.g. `x := ?bits[8]`).
            Expr::OptionTy(_) => {
                self.error(
                    self.expr_span(id),
                    "`?T` is a type, not a value".to_string(),
                );
                Ty::Unknown
            }
            Expr::Absent => Ty::AbsentLit,
            // Type-checks `inner` for its own sake (effects, diagnostics,
            // populating `expr_tys` for `check_assignable`'s later
            // lookup) but deliberately does NOT wrap `inner`'s `Ty` here
            // — see `Ty::Optional`'s own doc comment for why unification
            // needs to stay lazy, recursing through `check_assignable`
            // against the eventual TARGET's inner instead of a type this
            // arm precomputed.
            Expr::Optional(inner) => {
                self.type_expr(inner, locals);
                Ty::Optional(inner)
            }
            // `logic <expr>`: converts a fallible expression into a
            // plain boolean, always `bits[1]` regardless of `expr`'s own
            // type — `expr` is still type-checked normally (populating
            // `expr_tys`, running ordinary diagnostics), just its
            // resulting Ty is discarded here. Whether `expr` is actually
            // a fallible SHAPE (a fifo op, or a call to a guard-only
            // `<fails>` fn/impl) isn't a width/type question — checked
            // later in firrtl/checks.rs's `check_logic_args`, once
            // effects.rs's inferred signatures exist to consult,
            // matching where `check_failing_call_positions`/`check_
            // fifo_op_positions` already live for the same reason.
            Expr::Logic(inner) => {
                self.type_expr(inner, locals);
                Ty::Bits(Width::Known(1))
            }
        }
    }

    /// Solver-2 width rules. Modular arithmetic: `+`/`-`/bitwise keep the
    /// max width; `*` sums; shifts keep the left width; comparisons give
    /// bits[1]. `Int` absorbs into the other side.
    fn type_binop(&mut self, op: BinOp, l: Ty, r: Ty, lhs: ExprId, rhs: ExprId, at: ExprId) -> Ty {
        use BinOp::*;
        let span = self.expr_span(at);
        if matches!(op, Range | PlusColon | MinusColon) {
            // Only meaningful as a `Bracket`'s own argument (`type_bracket`
            // re-matches the raw AST shape there for the real width rule);
            // reached here only via the generic per-subexpression walk
            // `type_bracket` already does before that re-match, or if
            // used somewhere illegal (e.g. a bare `x := a +: b` statement)
            // — silently `Unknown` either way, same treatment `Range` has
            // always had. Emission's own generic `compile_binop` catch-all
            // still rejects an illegal bare use explicitly, so nothing
            // silently miscompiles.
            return Ty::Unknown;
        }
        // A comparison yields `l`'s own type/value on success (fails
        // otherwise) — Verse's `X > 0` semantics (TODO.md's "Comparisons
        // returning their left operand" design), the same "unwrap-or-
        // fail" shape `opt?`/`f.Deq[]` already have, NOT a standalone
        // `bits[1]` value anymore. Routed through the SAME `match (l, r)`
        // compatibility check every other operator gets below (so `a >
        // b` on incompatible types still errors, same as `a + b` would)
        // rather than short-circuiting before it the way this used to —
        // only the RESULT differs (`l`'s type, not the computed common
        // width). `l`'s original value is captured before the match
        // moves it in, since which arm actually matches doesn't change
        // what a comparison yields.
        let is_comparison = op.is_comparison();
        let l_ty = l.clone();
        // `a >>.! 300` (ast.rs's `lossy` set, populated by the parser
        // right where `.!` is written) — an explicit, per-application
        // opt-out of exactly the two checks below, `check_literal_fits`/
        // `check_shift_amount`. Computed once here rather than re-
        // queried per arm.
        let lossy = self.ast.lossy.contains(&at);
        match (l, r) {
            (Ty::Unknown, _) | (_, Ty::Unknown) => Ty::Unknown,
            (Ty::Int, Ty::Int) => {
                if is_comparison {
                    l_ty
                } else {
                    Ty::Int
                }
            }
            (Ty::Bits(w), Ty::Int) => {
                // The `Int` side absorbs `w` from its sibling (this arm's
                // whole point), but absorbing silently is exactly the gap
                // `check_assignable`'s own `(Ty::Int, Ty::Bits(_))` comment
                // promises is "range-checked at coercion" — this IS that
                // coercion site for a binary operand, the same way a
                // state write or port default is for an assignment. Without
                // this, `a + 100000000` (`a : [8]`) unified silently to
                // `[8]` with no diagnostic at all, not even at emission.
                //
                // EXCEPT for a shift: `Shl|Shr|AShr => Ty::Bits(a)` below
                // already says the shift AMOUNT'S width never enters the
                // result at all — it's a count, not a value bounded by
                // the shifted operand's own domain, so `check_literal_
                // fits` (a "does this VALUE fit" check) doesn't apply.
                // `check_shift_amount` (a differently-shaped "is this
                // COUNT too large" check) does.
                if !lossy {
                    if matches!(op, Shl | Shr | AShr) {
                        self.check_shift_amount(rhs, &Ty::Bits(w));
                    } else {
                        self.check_literal_fits(rhs, &Ty::Bits(w));
                    }
                }
                if is_comparison { l_ty } else { Ty::Bits(w) }
            }
            (Ty::Int, Ty::Bits(w)) => {
                if !lossy && !matches!(op, Shl | Shr | AShr) {
                    self.check_literal_fits(lhs, &Ty::Bits(w));
                }
                if is_comparison { l_ty } else { Ty::Bits(w) }
            }
            (Ty::Bits(a), Ty::Bits(b)) => {
                if is_comparison {
                    l_ty
                } else {
                    // A real `Bits` shift amount (a sized literal like
                    // `8'd20`, say, or any constant-foldable expression)
                    // is just as checkable as a bare `Int` one
                    // (`type_binop`'s `(Bits, Int)` arm, above) —
                    // `check_shift_amount` itself only needs a compile-
                    // time-constant VALUE, which `const_eval` finds the
                    // same way regardless of which `Ty` the amount's own
                    // expression happens to carry.
                    if matches!(op, Shl | Shr | AShr) && !lossy {
                        self.check_shift_amount(rhs, &Ty::Bits(a));
                    }
                    Ty::Bits(super::combine_bits_width(op, a, b))
                }
            }
            (l, r) => {
                self.error(
                    span,
                    format!("operator needs bits operands, got {l} and {r}"),
                );
                Ty::Unknown
            }
        }
    }

    /// Whether `id` is a call to a function whose computed effect
    /// signature can fail -- the `Stmt::IfLet` sibling of `checks.rs`'s
    /// identical `is_failing_call`/`call_target_fn` (that copy lives on
    /// `Emitter`, which can't see `TypeChecker`, hence the duplicate
    /// rather than a shared helper).
    pub(crate) fn is_failing_call(&self, id: ExprId) -> bool {
        let Expr::Call { callee, .. } = self.ast.expr(id) else {
            return false;
        };
        let Some(def) = self.res.expr_defs.get(callee) else {
            return false;
        };
        if !matches!(self.res.def(*def).kind, DefKind::Fn | DefKind::Impl) {
            return false;
        }
        self.res
            .item_defs
            .iter()
            .find(|(_, d)| *d == def)
            .is_some_and(|(item, _)| self.fx.sigs.get(item).is_some_and(|s| s.fails))
    }

    /// Brackets: memory read, bit/slice select, or fifo op.
    /// Whether `id` is a bare `fifo.Deq[]` -- the exact shape `type_
    /// bracket` recognizes as a fifo op, checked independently here
    /// since `Stmt::IfLet`'s arm needs to know this BEFORE deciding
    /// whether to call `type_expr` (which would otherwise just report
    /// `elem`'s type with no way to tell "a real fifo op" apart from any
    /// other `[N]`-typed expression). `Enq` is deliberately excluded --
    /// there is no value to bind an if-let's name to.
    pub(crate) fn is_fifo_deq(&self, id: ExprId) -> bool {
        let Expr::Bracket { callee, .. } = self.ast.expr(id) else {
            return false;
        };
        let Expr::Field { base, name } = self.ast.expr(*callee) else {
            return false;
        };
        if name != "Deq" {
            return false;
        }
        self.res
            .expr_defs
            .get(base)
            .is_some_and(|def| matches!(self.state_tys.get(def), Some(Ty::Fifo { .. })))
    }

    fn type_bracket(
        &mut self,
        id: ExprId,
        callee: ExprId,
        args: &[ExprId],
        locals: &mut HashMap<DefId, Ty>,
    ) -> Ty {
        // Fifo op: `f.Deq[]` / `f.Enq[x]`.
        if let Expr::Field { base, name } = self.ast.expr(callee).clone()
            && let Some(def) = self.res.expr_defs.get(&base).copied()
            && let Some(Ty::Fifo { elem, depth }) = self.state_tys.get(&def).cloned()
        {
            self.types.expr_tys.insert(
                callee,
                Ty::Fifo {
                    elem: elem.clone(),
                    depth,
                },
            );
            return match name.as_str() {
                "Deq" => {
                    if !args.is_empty() {
                        self.error(self.expr_span(id), "`Deq[]` takes no arguments".to_string());
                    }
                    *elem
                }
                "Enq" => {
                    if args.len() != 1 {
                        self.error(
                            self.expr_span(id),
                            "`Enq[x]` takes one argument".to_string(),
                        );
                        return Ty::Unit;
                    }
                    let arg = self.type_expr(args[0], locals);
                    self.check_assignable(&arg, &elem, args[0], "enqueue");
                    Ty::Unit
                }
                other => {
                    self.error(
                        self.expr_span(id),
                        format!("unknown fifo operation `{other}` (Deq or Enq)"),
                    );
                    Ty::Unknown
                }
            };
        }

        // A builtin used with brackets (`sync[h1, h2]`, `race[h1, h2]`) —
        // brackets mark fallibility, matching `f.Deq[]`/`f.Enq[x]`, so a
        // fallible builtin is called this way rather than with parens.
        // Dispatches through the same per-builtin type rule an ordinary
        // (infallible) `name(args)` call already uses.
        if let Some(def) = self.res.expr_defs.get(&callee).copied()
            && self.res.def(def).kind == DefKind::Builtin
        {
            let name = self.res.def(def).name.clone();
            let arg_tys: Vec<Ty> = args.iter().map(|a| self.type_expr(*a, locals)).collect();
            return self.type_builtin_call(id, &name, args, &arg_tys);
        }

        let base = self.type_expr(callee, locals);
        for a in args {
            self.type_expr(*a, locals);
        }
        match base {
            Ty::Mem { elem, .. } => {
                if args.len() != 1 {
                    self.error(
                        self.expr_span(id),
                        "memory read takes one index".to_string(),
                    );
                }
                *elem
            }
            Ty::Bits(w) => {
                let _ = w;
                // Bit select (`x[i]`), slice (`x[hi..lo]`), or indexed
                // part-select (`x[base +: width]`/`x[base -: width]`) —
                // three shapes, each with a genuinely different width
                // rule, distinguished by the argument's own AST shape.
                if let Some(&arg) = args.first()
                    && let Expr::Binary { op, lhs, rhs } = self.ast.expr(arg).clone()
                {
                    match op {
                        // A slice's width can ONLY be known if BOTH
                        // bounds are compile-time constants — FIRRTL's
                        // `bits` primop needs static bounds, and there's
                        // no way to express a dynamically-SIZED result
                        // in this language's type system at all (unlike
                        // a single index, whose width is always exactly
                        // 1 regardless of whether it's dynamic). A
                        // non-const bound here used to silently fall
                        // through to the `Ty::Bits(Width::Known(1))`
                        // fallback below — a real latent mistyping bug
                        // (a narrower-than-declared value passes
                        // `check_assignable` without complaint) — now an
                        // explicit error instead.
                        BinOp::Range => {
                            return match (
                                self.const_eval(lhs, &HashMap::new()),
                                self.const_eval(rhs, &HashMap::new()),
                            ) {
                                (Some(hi), Some(lo)) => Ty::Bits(Width::Known(hi.abs_diff(lo) + 1)),
                                _ => {
                                    self.error(
                                        self.expr_span(id),
                                        "a slice's bounds (`x[hi..lo]`) must both be \
                                         compile-time constants (this language has no \
                                         way to express a dynamically-sized result); \
                                         use indexed part-select for a dynamic start \
                                         with a fixed width instead — `x[base +: \
                                         width]`/`x[base -: width]`"
                                            .to_string(),
                                    );
                                    Ty::Unknown
                                }
                            };
                        }
                        // Indexed part-select: `base` may be anything —
                        // its own width is irrelevant to the RESULT's
                        // width, which is fixed entirely by `width`, so
                        // (unlike a slice) this only ever needs ONE side
                        // to be a compile-time constant.
                        BinOp::PlusColon | BinOp::MinusColon => {
                            return match self.const_eval(rhs, &HashMap::new()) {
                                Some(width) if width >= 1 => Ty::Bits(Width::Known(width)),
                                Some(_) => {
                                    self.error(
                                        self.expr_span(id),
                                        format!(
                                            "indexed part-select (`{}`) width must be \
                                             at least 1",
                                            op.symbol()
                                        ),
                                    );
                                    Ty::Unknown
                                }
                                None => {
                                    self.error(
                                        self.expr_span(id),
                                        format!(
                                            "indexed part-select (`{}`) needs a \
                                             compile-time constant width",
                                            op.symbol()
                                        ),
                                    );
                                    Ty::Unknown
                                }
                            };
                        }
                        _ => {}
                    }
                }
                // A single index — `x[i]` — is always exactly 1 bit,
                // whether `i` is a compile-time constant or a genuine
                // runtime value; unlike a slice, there's no ambiguity
                // about the result's width to resolve either way.
                Ty::Bits(Width::Known(1))
            }
            // `xs[i]` (an element) or `xs[..mid]`/`xs[mid..]` (a
            // sub-list, `Expr::Range` — the one-sided form, exclusively
            // used for list slicing, never `BinOp::Range`'s two-sided
            // bit-slice shape). Both bounds/the index are checked for
            // being elaboration-time constants by the interpreter
            // (`firrtl/elaborate.rs`) once a real call site exists, not
            // here — this pass only needs the SHAPE, since a `list[T]`
            // body is checked once, generically, independent of any
            // call site's actual length (exactly like a generic
            // `bits[N]` body).
            Ty::List(elem) => {
                if args.len() != 1 {
                    self.error(
                        self.expr_span(id),
                        "list index takes one argument".to_string(),
                    );
                    return Ty::Unknown;
                }
                if matches!(self.ast.expr(args[0]), Expr::Range { .. }) {
                    Ty::List(elem)
                } else {
                    *elem
                }
            }
            Ty::Unknown => Ty::Unknown,
            other => {
                self.error(self.expr_span(id), format!("cannot index {other}"));
                Ty::Unknown
            }
        }
    }

    /// Solver-1 instantiation: builtins by signature; user fns match
    /// implicit width params against concrete argument widths, then the
    /// return type evaluates under that solution.
    /// `hint` (new): the caller's own expected type for this WHOLE call's
    /// result, if one is genuinely known before this call is typed --
    /// today only `type_expr_with_hint`'s two callers (`Stmt::Assign`'s
    /// LHS declared type, `Stmt::Return`'s own `ret`) ever supply one,
    /// `None` everywhere else (a nested call, any other position) --
    /// deliberately narrow, not general top-down propagation. Lets
    /// `DefKind::Fn`'s own `env` solve an implicit width param from the
    /// RESULT side (unifying `hint` against `ret`'s own width expression
    /// via `invert_implicit_width`) in addition to the arg-width solve
    /// already below -- needed for `Double(trunc(b))`, where `trunc`'s
    /// own 1-argument form has no width of its own to contribute an arg
    /// width from at all (`Ty::Bits(Width::Unknown)`, deferred all the
    /// way to FIRRTL emission's own separate hint mechanism through v22)
    /// but the caller's declared write-target width (`b : [5]`) DOES
    /// pin `n = 4` via `Double`'s own `[n + 1]` return type, with no
    /// value-level reasoning involved at all -- see this fn's own
    /// backward-fill step below for why that solved `n` also needs
    /// writing back into `trunc(b)`'s own `expr_tys` entry, not just
    /// used locally here, or FIRRTL emission's OWN independent width
    /// resolver would still see `Width::Unknown` and fail regardless of
    /// types.rs itself now accepting the program.
    fn type_call(
        &mut self,
        id: ExprId,
        callee: ExprId,
        args: &[ExprId],
        locals: &mut HashMap<DefId, Ty>,
        hint: Option<&Ty>,
    ) -> Ty {
        let mut arg_tys: Vec<Ty> = args.iter().map(|a| self.type_expr(*a, locals)).collect();
        let Some(def) = self.res.expr_defs.get(&callee).copied() else {
            return Ty::Unknown;
        };
        let def_info = self.res.def(def).clone();
        match def_info.kind {
            DefKind::Builtin => self.type_builtin_call(id, &def_info.name, args, &arg_tys),
            DefKind::Fn | DefKind::Impl | DefKind::Spec => {
                let Some(item) = self.def_items.get(&def).copied() else {
                    return Ty::Unknown;
                };
                let Item::Fn { params, ret, .. } = self.ast.item(item).clone() else {
                    return Ty::Unknown;
                };
                if params.len() != args.len() {
                    self.error(
                        self.expr_span(id),
                        format!(
                            "`{}` takes {} argument(s), got {}",
                            def_info.name,
                            params.len(),
                            args.len()
                        ),
                    );
                    return Ty::Unknown;
                }
                let mut env: HashMap<DefId, u64> = HashMap::new();
                // Match `bits[N]` params against known arg widths FIRST
                // -- this precise, bottom-up solve always takes priority
                // over the hint-based fallback below.
                for (param, arg_ty) in params.iter().zip(&arg_tys) {
                    if let Some(pdef) = self.implicit_width_param(param.ty)
                        && let Ty::Bits(Width::Known(w)) = arg_ty
                        && let Some(prev) = env.insert(pdef, *w)
                        && prev != *w
                    {
                        self.error(
                            self.expr_span(id),
                            format!("conflicting widths for implicit parameter: {prev} vs {w}"),
                        );
                    }
                }
                // Solve from the caller's own expected-type hint LAST,
                // and ONLY to fill a param no argument resolved --
                // found necessary via a real test regression, not
                // assumed: an earlier version ran this FIRST and let a
                // disagreeing argument "conflict" with it, but the
                // relationship between a call's RETURN type and its
                // hint is ASSIGNABILITY (widening allowed), not
                // equality, so forcing it as an exact constraint on a
                // param ALSO tied to a concrete argument is simply
                // wrong -- confirmed by `a_nested_generic_calls_shared_
                // implicit_width_param_conflict_is_a_clean_error_not_a_
                // guess` (tests/firrtl.rs): `Bar(a : [p], ..) : [p]`
                // called as `Bar(c, d)` with `c : [4]` assigned to an
                // `[8]`-wide target legitimately solves `p = 4` from
                // `c`, then widens the (narrower) result to fit -- NOT
                // a `p = 8` vs `p = 4` conflict. The hint only matters
                // for a param like `Double`'s own `n` above, which no
                // argument constrains at all (`trunc(b)`'s own width is
                // `Width::Unknown` until THIS solve fills it in).
                if let (Some(Ty::Bits(Width::Known(target))), Some(ret_expr)) = (hint, ret)
                    && let Some(width_expr) =
                        super::eval::bits_width_expr(self.ast, self.res, ret_expr)
                    && let Some((pdef, w)) =
                        super::eval::invert_implicit_width(self.ast, self.res, width_expr, *target)
                {
                    env.entry(pdef).or_insert(w);
                }
                // Backward-fill: an argument that's EXACTLY a 1-argument
                // `trunc(value)` call still carries `Width::Unknown`
                // (its own width was never resolvable bottom-up) -- if
                // its OWN param type is now solvable from `env` (either
                // source above), record that as `trunc`'s real width,
                // both locally (`arg_tys[i]`, for the assignability
                // check right below) AND in `expr_tys` (`self.types.
                // expr_tys`, what FIRRTL emission's own width resolver
                // actually reads) -- NOT a general hint-propagation
                // mechanism, deliberately scoped to this one shape,
                // exactly like every other "opt-in, not blanket" cut in
                // this pass.
                for (i, (param, arg)) in params.iter().zip(args).enumerate() {
                    if matches!(arg_tys[i], Ty::Bits(Width::Unknown))
                        && self.is_one_arg_trunc_call(*arg)
                        && let Ty::Bits(Width::Known(w)) = self.eval_ty(param.ty, &env)
                    {
                        let resolved = Ty::Bits(Width::Known(w));
                        self.types.expr_tys.insert(*arg, resolved.clone());
                        arg_tys[i] = resolved;
                    }
                }
                // Check each arg against its (instantiated) param type.
                for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
                    let pty = self.eval_ty(param.ty, &env);
                    self.check_assignable(arg_ty, &pty, *arg, "argument");
                }
                match ret {
                    Some(r) => self.eval_ty(r, &env),
                    None => Ty::Unit,
                }
            }
            // No `DefKind::Rule` arm: a rule name lives in resolve.rs's
            // own separate `rule_scopes` namespace now, never `expr_defs`
            // (see that field's doc comment) — this callee can never
            // resolve to one, so there's nothing to reject here.
            DefKind::Reg
            | DefKind::Mem
            | DefKind::Fifo
            | DefKind::Input
            | DefKind::Output
            | DefKind::Module => {
                self.error(
                    self.expr_span(id),
                    format!(
                        "`{}` is {}, not callable",
                        def_info.name,
                        def_info.kind.describe()
                    ),
                );
                Ty::Unknown
            }
            _ => Ty::Unknown,
        }
    }

    fn type_builtin_call(&mut self, id: ExprId, name: &str, args: &[ExprId], arg_tys: &[Ty]) -> Ty {
        match name {
            "clog2" | "len" => Ty::Int,
            // `max(a, b, ...)`/`min(a, b, ...)`: dual-purpose, unlike
            // every other builtin here. When EVERY argument is `Ty::Int`
            // (bare integer literals, `clog2(..)` results, implicit
            // width params — the same shapes a type-position width
            // expression is built from), this stays compile-time-only,
            // same class as `clog2`: typed `Ty::Int`, foldable directly
            // inside `bits[max(n, m)]` via `const_eval_expr` (types/
            // eval.rs), not synthesizable as an ordinary call. The
            // moment at least one argument is a real `Bits` VALUE (a
            // port, register, or anything else with runtime width), this
            // is a synthesizable comparator+mux instead (`compile_max_
            // min`, firrtl/calls.rs) — same "`Int` absorbs into `Bits`,
            // checked to fit" rule `type_binop`'s own `(Bits, Int)` arm
            // already uses for a bare arithmetic operand, applied
            // pairwise across however many arguments there are. Arity IS
            // checked here (at least two arguments — a one-argument
            // `max`/`min` is just its argument, not a meaningful call).
            "max" | "min" => {
                if args.len() < 2 {
                    self.error(
                        self.expr_span(id),
                        format!("`{name}` takes at least two arguments"),
                    );
                    return Ty::Unknown;
                }
                if arg_tys.iter().all(|t| matches!(t, Ty::Int)) {
                    return Ty::Int;
                }
                // `max`'s result can need as many bits as its WIDEST
                // `Bits` argument (the larger of a `[4]` and an `[8]` can
                // need all 8 bits) — but `min`'s result can never exceed
                // its NARROWEST argument's own domain (`min(a, b) <= a`
                // and `<= b`, always), so it only ever needs the
                // NARROWEST argument's width. Using `max` for both here
                // would reject a perfectly legal `min(a, b)` write into a
                // target no wider than the smaller operand — a `min`
                // result can never actually overflow that.
                let combine = if name == "max" { u64::max } else { u64::min };
                let mut width: Option<Width> = None;
                for ty in arg_tys {
                    match ty {
                        Ty::Bits(w) => {
                            width = Some(match width {
                                None => *w,
                                Some(Width::Known(a)) => match w {
                                    Width::Known(b) => Width::Known(combine(a, *b)),
                                    Width::Unknown => Width::Unknown,
                                },
                                Some(Width::Unknown) => Width::Unknown,
                            });
                        }
                        Ty::Int => {}
                        Ty::Unknown => return Ty::Unknown,
                        other => {
                            self.error(
                                self.expr_span(id),
                                format!("`{name}` needs `bits`/`int` arguments, got {other}"),
                            );
                            return Ty::Unknown;
                        }
                    }
                }
                let result = Ty::Bits(width.expect("checked not all Ty::Int above"));
                if let Ty::Bits(Width::Known(_)) = &result {
                    for (arg, ty) in args.iter().zip(arg_tys) {
                        if matches!(ty, Ty::Int) {
                            self.check_literal_fits(*arg, &result);
                        }
                    }
                }
                result
            }
            // `trunc(value, width)`: the ordinary explicit form, typed
            // directly from the const-evaluated `width` argument, same
            // as always.
            //
            // `trunc(value)`, ONE argument: width INFERRED from wherever
            // this call's own result is used, not spelled out here. Bottom-
            // up type-checking (this whole pass) has no way to know that
            // yet — `Ty::Bits(Width::Unknown)` is the honest answer at
            // this point, not a placeholder to fill in later. Two existing
            // mechanisms pick up from there without any new machinery:
            // `check_assignable`/`check_literal_fits` only ever compare
            // `Width::Known` widths, so `Unknown` here already means
            // "don't flag a mismatch, not enough information" everywhere
            // they're called — a real width IS still enforced, just later
            // and elsewhere: FIRRTL emission's `compile_trunc`
            // (src/firrtl/calls.rs) resolves the width from `hint`, its
            // OWN top-down "what does this expression's result need to be"
            // parameter, threaded through the emitter from every write
            // target/return type/etc. already — the concrete counterpart
            // to this type-checking pass's own top-down blind spot. No
            // hint reaching that call site is `compile_trunc`'s own clean,
            // separate error, not this function's.
            "trunc" => match args.len() {
                1 => Ty::Bits(Width::Unknown),
                2 => match self.const_eval(args[1], &HashMap::new()) {
                    Some(w) => Ty::Bits(Width::Known(w)),
                    None => Ty::Bits(Width::Unknown),
                },
                _ => {
                    self.error(
                        self.expr_span(id),
                        "`trunc` takes (value) or (value, width)".to_string(),
                    );
                    Ty::Unknown
                }
            },
            "pack" => {
                let mut total = 0u64;
                for t in arg_tys {
                    match t {
                        Ty::Bits(Width::Known(w)) => total += w,
                        _ => return Ty::Bits(Width::Unknown),
                    }
                }
                Ty::Bits(Width::Known(total))
            }
            // `zext(value, width)`/`sext(value, width)`: widen `value` to
            // exactly `width` bits, zero- or sign-filling the new high
            // bits. Unlike `trunc`, there's no 1-argument inferred-width
            // form — the whole point of spelling one of these out (over
            // just letting the value flow into a wider write target,
            // which already zero-extends implicitly) is to make a
            // SIGN-extension explicit, so `width` is always required.
            // `target < value`'s own width is a real error, not silently
            // accepted as a no-op or routed through `trunc` — narrowing
            // is a different operation with different data loss, and the
            // fix is named right in the message.
            "zext" | "sext" => {
                if args.len() != 2 {
                    self.error(self.expr_span(id), format!("`{name}` takes (value, width)"));
                    return Ty::Unknown;
                }
                let target = self.const_eval(args[1], &HashMap::new());
                match (arg_tys.first(), target) {
                    (Some(Ty::Bits(Width::Known(vw))), Some(tw)) => {
                        if tw < *vw {
                            self.error(
                                self.expr_span(id),
                                format!(
                                    "`{name}`'s target width ({tw}) is narrower than its \
                                     value's own width ({vw}); use `trunc` to narrow instead"
                                ),
                            );
                        }
                        Ty::Bits(Width::Known(tw))
                    }
                    (_, Some(tw)) => Ty::Bits(Width::Known(tw)),
                    _ => Ty::Bits(Width::Unknown),
                }
            }
            "prio" => match arg_tys.first() {
                Some(Ty::Bits(Width::Known(w))) => {
                    Ty::Bits(Width::Known(super::prio_result_width(*w)))
                }
                _ => Ty::Bits(Width::Unknown),
            },
            // `popcount(bits)`: the number of set bits, widened to fit
            // the worst case (see `popcount_result_width`'s own doc
            // comment on the `+1`).
            "popcount" => {
                if args.len() != 1 {
                    self.error(
                        self.expr_span(id),
                        "`popcount` takes one argument".to_string(),
                    );
                    return Ty::Unknown;
                }
                match arg_tys.first() {
                    Some(Ty::Bits(Width::Known(w))) => {
                        Ty::Bits(Width::Known(super::popcount_result_width(*w)))
                    }
                    _ => Ty::Bits(Width::Unknown),
                }
            }
            // `reverse(bits)`: same width as its argument, bit order
            // flipped.
            "reverse" => {
                if args.len() != 1 {
                    self.error(
                        self.expr_span(id),
                        "`reverse` takes one argument".to_string(),
                    );
                    return Ty::Unknown;
                }
                match arg_tys.first() {
                    Some(Ty::Bits(Width::Known(w))) => Ty::Bits(Width::Known(*w)),
                    _ => Ty::Bits(Width::Unknown),
                }
            }
            // `rotl(value, n)`/`rotr(value, n)`: same width as `value` —
            // rotating never changes bit count, unlike `zext`/`trunc`.
            // `n` itself isn't checked here at all (same stance `trunc`'s
            // 2-arg width takes): it only matters at FIRRTL emission,
            // which requires it to be a compile-time constant (a static
            // bit-slice boundary, not a dynamically indexed one) and
            // reports that there, not here, the same division of labor
            // `compile_trunc`'s doc comment already explains for its own
            // one-arg width-inference gap.
            "rotl" | "rotr" => {
                if args.len() != 2 {
                    self.error(
                        self.expr_span(id),
                        format!("`{name}` takes (value, amount)"),
                    );
                    return Ty::Unknown;
                }
                match arg_tys.first() {
                    Some(Ty::Bits(Width::Known(w))) => Ty::Bits(Width::Known(*w)),
                    _ => Ty::Bits(Width::Unknown),
                }
            }
            // `mux(sel, a, b)`: an explicit 2-way combinational select —
            // `sel` must be exactly `bits[1]` (no implicit "nonzero
            // selects a" truthiness; `prio` takes the same strict stance
            // on its own callers gating explicitly, see its own doc
            // comment), `a`/`b` must both be `Bits`. Result width is the
            // max of the two arms, mirroring `combine_bits_width`'s
            // ordinary Chisel-style rule for every other binary op that
            // isn't `Mul`/a shift — and matching FIRRTL's own `mux`
            // primop, which pads the narrower arm to the wider one
            // automatically, so no explicit padding is needed at
            // emission either.
            "mux" => {
                if args.len() != 3 {
                    self.error(self.expr_span(id), "`mux` takes (sel, a, b)".to_string());
                    return Ty::Unknown;
                }
                if let Some(sel_ty) = arg_tys.first()
                    && !matches!(sel_ty, Ty::Bits(Width::Known(1)) | Ty::Bits(Width::Unknown))
                {
                    self.error(
                        self.expr_span(args[0]),
                        format!(
                            "`mux`'s selector must be `bits[1]`, not {sel_ty}; wrap a \
                             comparison with `logic` to get one"
                        ),
                    );
                }
                match (arg_tys.get(1), arg_tys.get(2)) {
                    (Some(Ty::Bits(Width::Known(a))), Some(Ty::Bits(Width::Known(b)))) => {
                        Ty::Bits(Width::Known((*a).max(*b)))
                    }
                    (Some(Ty::Bits(_)), Some(Ty::Bits(_))) => Ty::Bits(Width::Unknown),
                    _ => {
                        self.error(
                            self.expr_span(id),
                            "`mux`'s two arms must both be `bits[..]`".to_string(),
                        );
                        Ty::Unknown
                    }
                }
            }
            "sync" => Ty::Unit,
            // Guard-only (`race[...]` as its own statement) never reads
            // this type; value-producing (`value := race[...]`) does —
            // the winner's own result type, once every named handle is
            // confirmed to share one. Args are handles (`Ty::Handle(T)`),
            // never the flattened internal form below (that's a DIFFERENT
            // builtin name, `__race_value`, never spelled `race`).
            "race" => {
                let mut result: Option<Ty> = None;
                for (arg, ty) in args.iter().zip(arg_tys) {
                    match ty {
                        Ty::Handle(inner) => match &result {
                            None => result = Some((**inner).clone()),
                            Some(prev) if prev != inner.as_ref() => {
                                self.error(
                                    self.expr_span(*arg),
                                    format!(
                                        "`race`'s handles must all share the same result \
                                         type; this one is {inner}, an earlier one was {prev}"
                                    ),
                                );
                            }
                            _ => {}
                        },
                        Ty::Unknown => {}
                        other => {
                            self.error(
                                self.expr_span(*arg),
                                format!("`race` needs a spawned handle, not {other}"),
                            );
                        }
                    }
                }
                result.unwrap_or(Ty::Unit)
            }
            // `__race_value[d1, r1, d2, r2, ...]` — lower.rs's own
            // rewrite of a value-producing `race[...]` at render time:
            // alternating done-flag/result-value pairs, already-renamed
            // real registers, never written by a user. `race`'s own
            // dispatch above already confirmed every result shares one
            // type before this was ever generated, so this just reads it
            // back off the first result arg — no arity/shape validation
            // a real source mistake could ever trigger here.
            "__race_value" => arg_tys.get(1).cloned().unwrap_or(Ty::Unknown),
            // any(range) is a model-checker free variable.
            "any" => Ty::Bits(Width::Unknown),
            // `map(xs, f)` (DESIGN.md's "Closures and partial
            // application"): only meaningful inside `<elaborates>` code,
            // where `elaborate.rs`'s own interpreter evaluates it
            // directly and the whole call disappears from the spliced
            // source before this type ever matters downstream -- this
            // case exists so a wrong-arity/non-list call still gets a
            // clean error at the USUAL point instead of only surfacing
            // later, and so the result carries a real element type
            // (`f`'s own already-inferred type, per `_`'s
            // `Ty::Unknown`-poisons-silently pass-through above --
            // NOT `xs`'s own element type, which a type-changing
            // closure would make wrong).
            "map" => {
                if args.len() != 2 {
                    self.error(
                        self.expr_span(id),
                        format!("`map` takes exactly two arguments, got {}", args.len()),
                    );
                    return Ty::Unknown;
                }
                match &arg_tys[0] {
                    Ty::List(_) | Ty::Unknown => Ty::List(Box::new(arg_tys[1].clone())),
                    other => {
                        self.error(
                            self.expr_span(id),
                            format!("`map`'s first argument must be a list, got {other}"),
                        );
                        Ty::Unknown
                    }
                }
            }
            _ => Ty::Unknown,
        }
    }
}
