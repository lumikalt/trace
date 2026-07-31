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
                    self.compile_bit_select(callee, &args)
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
                             simple body — `let` bindings, `if`/`else` branches, and a \
                             trailing `return`, no nested user-function calls — can be \
                             inlined; or a call to the builtin `prio`)"
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

    /// `x[i]` or `x[hi..lo]` on a bits-typed (not memory) base. FIRRTL's
    /// `bits` primop needs static bounds, so both forms require literal
    /// integer indices — a computed bound is a v0 restriction, not a
    /// missing feature the type checker would otherwise reject (it
    /// happily types a dynamic single-bit select as `bits[1]`).
    pub(crate) fn compile_bit_select(
        &mut self,
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
        let bounds = if let Expr::Binary {
            op: BinOp::Range,
            lhs,
            rhs,
        } = self.ast.expr(arg)
        {
            // `const_eval` already accepts a bare `Int` or a sized
            // literal (`8'd3`) equally — no new literal-recognition
            // logic needed here, just reusing it instead of matching
            // `Expr::Int`/`Expr::SizedInt` by hand.
            match (self.const_eval(*lhs), self.const_eval(*rhs)) {
                (Some(hi), Some(lo)) => Some((hi, lo)),
                _ => None,
            }
        } else {
            self.const_eval(arg).map(|i| (i, i))
        };
        let Some((hi, lo)) = bounds else {
            self.error(
                self.ast.expr_spans[arg.0 as usize].clone(),
                "bit-select/slice bounds must be literal integers in FIRRTL emission \
                 (v0 restriction: no computed bit-select bounds)"
                    .to_string(),
            );
            return Err(());
        };
        if hi < lo {
            self.error(
                self.ast.expr_spans[arg.0 as usize].clone(),
                format!(
                    "slice bounds must be high..low (got {hi}..{lo}); the type checker \
                     accepts either order but FIRRTL's `bits` primop needs hi >= lo"
                ),
            );
            return Err(());
        }
        Ok(format!("bits({base}, {hi}, {lo})"))
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
            UnOp::BitNot => {
                let w = self.width_of(id);
                let e = self.compile_expr_hinted(operand, Some(w))?;
                Ok(format!("not({e})"))
            }
            UnOp::Not => {
                self.error(
                    self.ast.expr_spans[id.0 as usize].clone(),
                    "logical `!` is not yet supported in FIRRTL emission (v0 \
                     restriction: use `~` for bitwise complement, or a comparison)"
                        .to_string(),
                );
                Err(())
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
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
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
                     restriction: div and rem are not supported)"
                        .to_string(),
                );
                return Err(());
            }
        })
    }

    /// Static (literal-amount) shifts only — FIRRTL's `shl`/`shr` need a
    /// constant, and a dynamic-amount `dshl`/`dshr` isn't wired up yet
    /// (v0 restriction). `shl` grows the width by the shift amount and
    /// `shr` shrinks it, but types.rs keeps the left operand's width for
    /// both, matching Verilog's fixed-width `<<`/`>>` — so both are
    /// brought back to that width: `shl` by dropping the high bits that
    /// fell off, `shr` by zero-padding back up.
    pub(crate) fn compile_shift(
        &mut self,
        op: BinOp,
        lhs: ExprId,
        rhs: ExprId,
    ) -> Result<String, ()> {
        let Some(n) = self.const_eval(rhs) else {
            self.error(
                self.ast.expr_spans[rhs.0 as usize].clone(),
                "shift amount must be a literal integer in FIRRTL emission (v0 \
                 restriction: no variable-amount shifts)"
                    .to_string(),
            );
            return Err(());
        };
        let w = self.width_of(lhs);
        let l = self.compile_expr_hinted(lhs, Some(w))?;
        Ok(match op {
            BinOp::Shl => format!("tail(shl({l}, {n}), {n})"),
            BinOp::Shr => format!("pad(shr({l}, {n}), {w})"),
            _ => unreachable!("compile_shift only called for Shl/Shr"),
        })
    }
}
