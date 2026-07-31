//! The generic expression compiler (`compile_expr`/`compile_expr_hinted`):
//! identifiers, literals, arithmetic/bitwise/shift/comparison operators,
//! bit-select/slice, memory reads, `instance.port` reads — the "expression
//! surface" mod.rs's module doc comment enumerates. Dispatches out to
//! calls.rs for `Expr::Call`; everything else's FIRRTL text is built
//! directly here.

use super::Emitter;
use super::fifo::*;
use crate::ast::{BinOp, Expr, ExprId, UnOp};
use crate::resolve::DefKind;
use crate::types::{Ty, Types, Width};

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
        if let Some(port) = self.read_ports.get(&id) {
            let Expr::Bracket { callee, .. } = self.ast.expr(id) else {
                unreachable!()
            };
            let Expr::Ident(mem_name) = self.ast.expr(*callee) else {
                unreachable!()
            };
            return Ok(format!("{mem_name}.{port}.data"));
        }
        if let Some((fifo, is_enq, _)) = self.fifo_op(id)
            && !is_enq
        {
            return Ok(fifo_data_name(&fifo));
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
        match self.ast.expr(id).clone() {
            Expr::Ident(_) => {
                let def = self.res.expr_defs.get(&id).copied();
                match def.map(|d| self.res.def(d).clone()) {
                    Some(d) if d.kind == DefKind::Output => Ok(self.output_regs[&d.name].clone()),
                    Some(d) if matches!(d.kind, DefKind::Local | DefKind::Param) => {
                        match self.locals.get(&def.unwrap()) {
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
                Ok(format!("UInt<{w}>({v})"))
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
            Expr::SizedInt { width, value } => Ok(format!("UInt<{width}>({value})")),
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
            // `!` and `~` emit the IDENTICAL FIRRTL `not` primop — types.rs
            // already made them a real, distinct operator, not pure
            // aliasing at this layer's expense: `!`'s own typing rule
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

    /// A literal operand has no width of its own; it absorbs one from
    /// its sibling. Arithmetic already has the absorbed width recorded
    /// on the whole expression (`types.expr_tys[id]`); comparisons
    /// don't (their own type is always `bits[1]`), so fall back to
    /// whichever side is a concrete, non-literal type.
    pub(crate) fn compile_binop(
        &mut self,
        id: ExprId,
        op: BinOp,
        lhs: ExprId,
        rhs: ExprId,
    ) -> Result<String, ()> {
        if matches!(op, BinOp::Shl | BinOp::Shr) {
            return self.compile_shift(op, lhs, rhs);
        }
        let known_width = |types: &Types, e: ExprId| {
            types.expr_tys.get(&e).and_then(|t| match t {
                Ty::Bits(Width::Known(w)) => Some(*w),
                _ => None,
            })
        };
        // For every op below, one side being a bare literal (`Ty::Int`)
        // means the checker typed the whole expression as the *other*
        // side's own width (types.rs's mixed-operand rule), so hinting
        // the literal to `known_width(id)` always lands on the right
        // value — whether or not this op is one whose "both sides bits"
        // rule also happens to equal that width (it does for every op
        // here except Mul, handled below).
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
            known_width(self.types, id)
        } else if matches!(self.ast.expr(lhs), Expr::Int(_)) {
            known_width(self.types, rhs)
        } else if matches!(self.ast.expr(rhs), Expr::Int(_)) {
            known_width(self.types, lhs)
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
                    known_width(self.types, lhs)
                }
                .unwrap_or(1);
                let wr = if matches!(self.ast.expr(rhs), Expr::Int(_)) {
                    hint
                } else {
                    known_width(self.types, rhs)
                }
                .unwrap_or(1);
                let target = known_width(self.types, id).unwrap_or(wl + wr);
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
                    known_width(self.types, lhs)
                }
                .unwrap_or(1);
                let target = known_width(self.types, id).unwrap_or(wl);
                match target.checked_sub(wl) {
                    Some(pad) if pad > 0 => format!("pad(div({l}, {r}), {target})"),
                    _ => format!("div({l}, {r})"),
                }
            }
            BinOp::Rem => {
                let wl = if matches!(self.ast.expr(lhs), Expr::Int(_)) {
                    hint
                } else {
                    known_width(self.types, lhs)
                }
                .unwrap_or(1);
                let wr = if matches!(self.ast.expr(rhs), Expr::Int(_)) {
                    hint
                } else {
                    known_width(self.types, rhs)
                }
                .unwrap_or(1);
                let firrtl_w = wl.min(wr);
                let target = known_width(self.types, id).unwrap_or(firrtl_w);
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

    /// Both static (literal-amount) and dynamic (runtime-amount) shifts.
    /// types.rs keeps the LEFT operand's width for `<<`/`>>` either way
    /// (matching Verilog's fixed-width shift semantics, not FIRRTL's own
    /// `shl`/`dshl`/`dshr`, which each grow or shrink) — so every case
    /// below ends by bringing the FIRRTL primop's own result back to
    /// `w`, the same shape whether the amount is known at compile time
    /// or not.
    ///
    /// Static: `shl` grows the width BY the (literal) shift amount,
    /// `shr` shrinks it by that same amount — brought back to `w` by
    /// dropping the high bits that fell off (`shl`) or zero-padding back
    /// up (`shr`).
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
                _ => unreachable!("compile_shift only called for Shl/Shr"),
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
            _ => unreachable!("compile_shift only called for Shl/Shr"),
        })
    }
}
