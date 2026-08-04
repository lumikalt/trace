//! Compile-time/const width evaluation: turning a type-position
//! expression (`bits[N]`, `[depth]elem_ty`, a struct name, `?T`, ...)
//! into a `Ty`, and elaboration-time constant folding (`clog2`, integer
//! arithmetic, an implicit param already solved in `env`) wherever a
//! concrete width/depth/size needs to be known right now rather than
//! deferred. `env` throughout carries whichever implicit `bits[N]`
//! params a call site has already solved (see `type_call`, expr.rs).

use super::{Ty, TypeChecker, Width, clog2};
use crate::ast::{BinOp, Expr, ExprId};
use crate::resolve::{DefId, DefKind};
use std::collections::HashMap;

impl<'a> TypeChecker<'a> {
    /// A fifo's type is either a bare element type (`bits[8]`, depth 1)
    /// or `[depth]elem_ty` (parser.rs's `parse_fifo_depth_ty`), which
    /// parses to the exact same `Bracket { callee: elem_ty, args: [depth] }`
    /// shape a mem's postfix `elem_ty[depth]` would produce. Extracts
    /// elem/depth directly from that shape rather than routing through
    /// `eval_ty`'s generic Bracket fallthrough, which would wrap the
    /// result in `Ty::Mem` instead of the flat `(elem, depth)` a fifo
    /// needs.
    pub(crate) fn eval_fifo_ty(&mut self, id: ExprId, env: &HashMap<DefId, u64>) -> (Ty, u64) {
        if let Expr::Bracket { callee, args } = self.ast.expr(id).clone()
            && !self.is_builtin(callee, "bits")
            && !self.is_builtin(callee, "list")
        {
            let elem = self.eval_ty(callee, env);
            return match args.first().and_then(|a| self.const_eval(*a, env)) {
                Some(depth) => (elem, depth),
                None => {
                    self.error(
                        self.expr_span(id),
                        "fifo depth must be an elaboration-time constant".to_string(),
                    );
                    (elem, 1)
                }
            };
        }
        (self.eval_ty(id, env), 1)
    }

    /// Evaluate a type expression. `env` carries solved implicit params.
    pub(crate) fn eval_ty(&mut self, id: ExprId, env: &HashMap<DefId, u64>) -> Ty {
        match self.ast.expr(id).clone() {
            Expr::Bracket { callee, args } => {
                if self.is_builtin(callee, "bits") {
                    if args.len() != 1 {
                        self.error(self.expr_span(id), "`bits` takes one width".to_string());
                        return Ty::Unknown;
                    }
                    return match self.const_eval(args[0], env) {
                        Some(w) => Ty::Bits(Width::Known(w)),
                        None => Ty::Bits(Width::Unknown),
                    };
                }
                if self.is_builtin(callee, "list") {
                    if args.len() != 1 {
                        self.error(
                            self.expr_span(id),
                            "`list` takes one element type, e.g. `list[8]` or `list[Pair]`"
                                .to_string(),
                        );
                        return Ty::Unknown;
                    }
                    return Ty::List(Box::new(self.eval_elem_ty(args[0], env)));
                }
                let elem = self.eval_ty(callee, env);
                if matches!(elem, Ty::Unknown) {
                    return Ty::Unknown;
                }
                let Some(len) = args.first().and_then(|a| self.const_eval(*a, env)) else {
                    self.error(
                        self.expr_span(id),
                        "memory size must be an elaboration-time constant".to_string(),
                    );
                    return Ty::Unknown;
                };
                Ty::Mem {
                    elem: Box::new(elem),
                    len,
                }
            }
            // Tolerate builtin type constructors we do not model yet
            // (wire); recognize a struct name; reject unknown identifiers
            // as types.
            Expr::Ident(name) => {
                let def = self.res.expr_defs.get(&id).copied();
                match def.map(|d| self.res.def(d).kind) {
                    Some(DefKind::Builtin) => Ty::Unknown,
                    Some(DefKind::Struct) => Ty::Struct {
                        def: def.unwrap(),
                        name,
                    },
                    _ => {
                        self.error(self.expr_span(id), "expected a type here".to_string());
                        Ty::Unknown
                    }
                }
            }
            Expr::Call { .. } => Ty::Unknown,
            Expr::OptionTy(inner) => Ty::Option(Box::new(self.eval_ty(inner, env))),
            _ => {
                self.error(self.expr_span(id), "expected a type here".to_string());
                Ty::Unknown
            }
        }
    }

    /// `list[T]`'s own single argument, with one extra sugar `eval_ty`
    /// itself doesn't have: a bare width expression (`list[8]`,
    /// `list[N]`, `list[clog2(N)]`) is shorthand for `list[[N]]` (that
    /// is, `list[bits[N]]`) — sidesteps the double-bracket `list[[N]]`
    /// for the overwhelmingly common case, a list of plain bit-vectors,
    /// while `list[Pair]` (a list of some OTHER type, e.g. a struct)
    /// still spells its element type out in full, since only a
    /// `[N]`-shaped element has anything to abbreviate in the first
    /// place. Told apart the same way `parse_expr`'s own `[N]`-vs-list-
    /// literal split is: by shape, not position — an expression that's
    /// ALREADY type-shaped (a `Bracket`, an `OptionTy`, or an `Ident`
    /// naming a declared struct) evaluates as a type normally; anything
    /// else is treated as the width argument an implicit `[...]` would
    /// have wrapped, mirroring the `bits` arm's own Known/Unknown
    /// `const_eval` fallback above.
    fn eval_elem_ty(&mut self, id: ExprId, env: &HashMap<DefId, u64>) -> Ty {
        let already_a_type = match self.ast.expr(id) {
            Expr::Bracket { .. } | Expr::OptionTy(_) => true,
            Expr::Ident(_) => self
                .res
                .expr_defs
                .get(&id)
                .is_some_and(|d| self.res.def(*d).kind == DefKind::Struct),
            _ => false,
        };
        if already_a_type {
            return self.eval_ty(id, env);
        }
        match self.const_eval(id, env) {
            Some(w) => Ty::Bits(Width::Known(w)),
            None => Ty::Bits(Width::Unknown),
        }
    }

    /// Constant-evaluate an elaboration expression, if possible.
    pub(crate) fn const_eval(&self, id: ExprId, env: &HashMap<DefId, u64>) -> Option<u64> {
        match self.ast.expr(id) {
            Expr::Int(v) => Some(*v),
            Expr::SizedInt { value, .. } => Some(*value),
            Expr::Ident(_) => {
                let def = self.res.expr_defs.get(&id)?;
                env.get(def).copied()
            }
            Expr::Binary { op, lhs, rhs } => {
                let l = self.const_eval(*lhs, env)?;
                let r = self.const_eval(*rhs, env)?;
                match op {
                    BinOp::Add => l.checked_add(r),
                    BinOp::Sub => l.checked_sub(r),
                    BinOp::Mul => l.checked_mul(r),
                    BinOp::Div => l.checked_div(r),
                    BinOp::Rem => l.checked_rem(r),
                    BinOp::Shl => l.checked_shl(r as u32),
                    BinOp::Shr => l.checked_shr(r as u32),
                    _ => None,
                }
            }
            Expr::Call { callee, args } => {
                if self.is_builtin(*callee, "clog2") && args.len() == 1 {
                    Some(clog2(self.const_eval(args[0], env)?))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn is_builtin(&self, id: ExprId, name: &str) -> bool {
        self.res.expr_defs.get(&id).is_some_and(|d| {
            let def = self.res.def(*d);
            def.kind == DefKind::Builtin && def.name == name
        })
    }

    /// `bits[N]` where `N` is an implicit param -> that param's def.
    pub(crate) fn implicit_width_param(&self, ty: ExprId) -> Option<DefId> {
        let Expr::Bracket { callee, args } = self.ast.expr(ty) else {
            return None;
        };
        if !self.is_builtin(*callee, "bits") {
            return None;
        }
        let def = self.res.expr_defs.get(args.first()?)?;
        (self.res.def(*def).kind == DefKind::ImplicitParam).then_some(*def)
    }
}
