//! Compile-time/const width evaluation: turning a type-position
//! expression (`bits[N]`, `[depth]elem_ty`, a struct name, `?T`, ...)
//! into a `Ty`, and elaboration-time constant folding (`clog2`, integer
//! arithmetic, an implicit param already solved in `env`) wherever a
//! concrete width/depth/size needs to be known right now rather than
//! deferred. `env` throughout carries whichever implicit `bits[N]`
//! params a call site has already solved (see `type_call`, expr.rs).

use super::{Ty, TypeChecker, Width, clog2};
use crate::ast::{Ast, BinOp, Expr, ExprId};
use crate::resolve::{DefId, DefKind, Resolution};
use std::collections::HashMap;

fn is_builtin_ref(res: &Resolution, id: ExprId, name: &str) -> bool {
    res.expr_defs.get(&id).is_some_and(|d| {
        let def = res.def(*d);
        def.kind == DefKind::Builtin && def.name == name
    })
}

/// `bits[<width-expr>]` -> the width expression itself (`<width-expr>`),
/// whatever shape it takes -- a bare implicit param (`bits[N]`) or a
/// compound formula (`bits[n + m + 1]`, a generic function's own return
/// type). `implicit_width_param`, below, is the narrower case (the width
/// expr must ALSO be a bare implicit-param ident, the only shape a
/// PARAMETER type is allowed) built on top of this same shape check, so
/// there's one place that knows what a `bits[...]` type looks like, not
/// two.
pub(crate) fn bits_width_expr(ast: &Ast, res: &Resolution, ty: ExprId) -> Option<ExprId> {
    let Expr::Bracket { callee, args } = ast.expr(ty) else {
        return None;
    };
    if !is_builtin_ref(res, *callee, "bits") {
        return None;
    }
    args.first().copied()
}

/// Constant-evaluate an elaboration expression, if possible. A free
/// function (not a `TypeChecker` method) specifically so `firrtl`'s own
/// emission-time width resolver (`Emitter::resolve_bits_width`,
/// `src/firrtl/expr.rs` -- resolving a NESTED generic function call's own
/// result width, by evaluating ITS return type expression against an
/// `env` built from the caller's already-resolved argument widths) can
/// call the SAME evaluator `TypeChecker::const_eval` (below, now a thin
/// wrapper) uses, instead of an independently-maintained second copy --
/// the same "const_fold/const_eval drift" class of bug this codebase has
/// already found and fixed more than once.
pub(crate) fn const_eval_expr(
    ast: &Ast,
    res: &Resolution,
    id: ExprId,
    env: &HashMap<DefId, u64>,
) -> Option<u64> {
    match ast.expr(id) {
        Expr::Int(v) => Some(*v),
        Expr::SizedInt { value, .. } => Some(*value),
        Expr::Ident(_) => {
            let def = res.expr_defs.get(&id)?;
            env.get(def).copied()
        }
        Expr::Binary { op, lhs, rhs } => {
            let l = const_eval_expr(ast, res, *lhs, env)?;
            let r = const_eval_expr(ast, res, *rhs, env)?;
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
            if is_builtin_ref(res, *callee, "clog2") && args.len() == 1 {
                Some(clog2(const_eval_expr(ast, res, args[0], env)?))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// `bits[N]` where `N` is an implicit param -> that param's def. Free
/// function for the same reason `bits_width_expr`/`const_eval_expr`,
/// above, are: `firrtl::Emitter::resolve_bits_width` needs this exact
/// check (a nested generic call's own PARAMETER types, to build the
/// `env` its return type evaluates against) and shouldn't carry a second
/// copy of "what counts as an implicit width param" to drift out of sync
/// with `type_call`'s own use of it (types/expr.rs).
pub(crate) fn implicit_width_param(ast: &Ast, res: &Resolution, ty: ExprId) -> Option<DefId> {
    let width_expr = bits_width_expr(ast, res, ty)?;
    let def = res.expr_defs.get(&width_expr)?;
    (res.def(*def).kind == DefKind::ImplicitParam).then_some(*def)
}

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
        const_eval_expr(self.ast, self.res, id, env)
    }

    fn is_builtin(&self, id: ExprId, name: &str) -> bool {
        is_builtin_ref(self.res, id, name)
    }

    /// `bits[N]` where `N` is an implicit param -> that param's def.
    pub(crate) fn implicit_width_param(&self, ty: ExprId) -> Option<DefId> {
        implicit_width_param(self.ast, self.res, ty)
    }
}
