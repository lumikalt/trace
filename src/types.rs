//! Type and width checking: DESIGN.md's two solvers, kept separate.
//!
//! Solver 1 (shapes + instantiation): every expression gets a `Ty`. Calls
//! to functions with implicit width parameters (`bits[N]`) instantiate
//! them by matching concrete argument widths — the unification half.
//!
//! Solver 2 (widths): widths are computed with monotone rules (`+`/`-`
//! keep max width, `*` sums, comparisons give 1) and only *checked* where
//! both sides are known. Rebinding a local widens its width; bodies
//! re-type until the local table is stable, with a divergence cap.
//!
//! Generic bodies (widths depending on unsolved implicit params) are
//! shape-checked only; their widths check numerically at each concrete
//! call site after instantiation.
//!
//! Width rules follow Chisel-style modular arithmetic: `a + b` has width
//! `max(|a|,|b|)`, and an integer literal absorbs into the other
//! operand's width (it must fit). Writing a wider value into a narrower
//! register is an error that names `trunc` — no silent truncation.

use crate::ast::{Ast, BinOp, Expr, ExprId, Item, ItemId, Stmt, StmtId};
use crate::lexer::Span;
use crate::resolve::{DefId, DefKind, Resolution};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    Known(u64),
    /// Depends on an unsolved implicit parameter; checked at concrete
    /// call sites, not here.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ty {
    Bits(Width),
    Mem {
        elem: Box<Ty>,
        len: u64,
    },
    Fifo(Box<Ty>),
    /// Elaboration-time integer (literals, `int` params).
    Int,
    Unit,
    /// Recovery type: unifies with anything, silences cascades.
    Unknown,
}

impl std::fmt::Display for Ty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ty::Bits(Width::Known(w)) => write!(f, "bits[{w}]"),
            Ty::Bits(Width::Unknown) => write!(f, "bits[?]"),
            Ty::Mem { elem, len } => write!(f, "{elem}[{len}]"),
            Ty::Fifo(elem) => write!(f, "fifo of {elem}"),
            Ty::Int => write!(f, "int"),
            Ty::Unit => write!(f, "unit"),
            Ty::Unknown => write!(f, "?"),
        }
    }
}

#[derive(Debug, Default)]
pub struct Types {
    pub expr_tys: HashMap<ExprId, Ty>,
    /// Type of every local (`let`/fresh `:=`) once its body's fixed point
    /// is reached. Keyed by `DefId` since locals are declared per rule/fn.
    pub local_tys: HashMap<DefId, Ty>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeError {
    pub span: Span,
    pub message: String,
}

/// Re-type a body at most this many times waiting for local widths to
/// stabilize; hitting the cap means widths grow without bound.
const WIDEN_CAP: usize = 50;

pub fn check(ast: &Ast, res: &Resolution) -> (Types, Vec<TypeError>) {
    let mut checker = TypeChecker {
        ast,
        res,
        def_items: res.item_defs.iter().map(|(i, d)| (*d, *i)).collect(),
        state_tys: HashMap::new(),
        types: Types::default(),
        errors: Vec::new(),
        emit: false,
    };
    checker.collect_state();
    checker.check_all();
    (checker.types, checker.errors)
}

struct TypeChecker<'a> {
    ast: &'a Ast,
    res: &'a Resolution,
    def_items: HashMap<DefId, ItemId>,
    /// Declared type of every reg/mem/fifo def.
    state_tys: HashMap<DefId, Ty>,
    types: Types,
    errors: Vec<TypeError>,
    /// Errors are emitted only on the final, stable typing pass.
    emit: bool,
}

fn bits_needed(v: u64) -> u64 {
    (64 - v.leading_zeros() as u64).max(1)
}

fn clog2(v: u64) -> u64 {
    if v <= 1 {
        0
    } else {
        64 - (v - 1).leading_zeros() as u64
    }
}

impl<'a> TypeChecker<'a> {
    fn error(&mut self, span: Span, message: String) {
        if self.emit {
            self.errors.push(TypeError { span, message });
        }
    }

    fn expr_span(&self, id: ExprId) -> Span {
        self.ast.expr_spans[id.0 as usize].clone()
    }

    // --- declared state types ---

    fn collect_state(&mut self) {
        self.emit = true;
        let mut stack: Vec<ItemId> = self.ast.roots.clone();
        while let Some(id) = stack.pop() {
            let Some(def) = self.res.item_defs.get(&id).copied() else {
                if let Item::Module { items, .. } = self.ast.item(id) {
                    stack.extend(items.iter().copied());
                }
                continue;
            };
            match self.ast.item(id).clone() {
                Item::Module { items, .. } => stack.extend(items),
                Item::Reg { ty, init, .. } => {
                    let ty = self.eval_ty(ty, &HashMap::new());
                    if !matches!(ty, Ty::Bits(_) | Ty::Unknown) {
                        let span = self.ast.item_spans[id.0 as usize].clone();
                        self.error(span, format!("a reg holds bits, not {ty}"));
                    }
                    if let Some(init) = init {
                        self.check_literal_fits(init, &ty);
                    }
                    self.state_tys.insert(def, ty);
                }
                Item::Mem { ty, .. } => {
                    let ty = self.eval_ty(ty, &HashMap::new());
                    if !matches!(ty, Ty::Mem { .. } | Ty::Unknown) {
                        let span = self.ast.item_spans[id.0 as usize].clone();
                        self.error(
                            span,
                            format!(
                                "a mem needs an element type and a size (`bits[w][n]`), got {ty}"
                            ),
                        );
                    }
                    self.state_tys.insert(def, ty);
                }
                Item::Fifo { ty, .. } => {
                    let elem = self.eval_ty(ty, &HashMap::new());
                    self.state_tys.insert(def, Ty::Fifo(Box::new(elem)));
                }
                _ => {}
            }
        }
        self.emit = false;
    }

    /// Evaluate a type expression. `env` carries solved implicit params.
    fn eval_ty(&mut self, id: ExprId, env: &HashMap<DefId, u64>) -> Ty {
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
            // (wire, list); reject unknown identifiers as types.
            Expr::Ident(_) => {
                let def = self.res.expr_defs.get(&id);
                if def.is_some_and(|d| self.res.def(*d).kind == DefKind::Builtin) {
                    Ty::Unknown
                } else {
                    self.error(self.expr_span(id), "expected a type here".to_string());
                    Ty::Unknown
                }
            }
            Expr::Call { .. } => Ty::Unknown,
            _ => {
                self.error(self.expr_span(id), "expected a type here".to_string());
                Ty::Unknown
            }
        }
    }

    /// Constant-evaluate an elaboration expression, if possible.
    fn const_eval(&self, id: ExprId, env: &HashMap<DefId, u64>) -> Option<u64> {
        match self.ast.expr(id) {
            Expr::Int(v) => Some(*v),
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

    // --- bodies ---

    fn check_all(&mut self) {
        let mut stack: Vec<ItemId> = self.ast.roots.clone();
        while let Some(id) = stack.pop() {
            match self.ast.item(id) {
                Item::Module { items, .. } => stack.extend(items.iter().copied()),
                Item::Rule { .. } | Item::Fn { .. } => self.check_body(id),
                _ => {}
            }
        }
    }

    /// Type one body to a fixed point of the local-width table.
    fn check_body(&mut self, id: ItemId) {
        let (params, ret, body) = match self.ast.item(id).clone() {
            Item::Rule { body, .. } => (Vec::new(), None, body),
            Item::Fn {
                params, ret, body, ..
            } => (params, ret, body),
            _ => return,
        };
        let empty = HashMap::new();
        let mut locals: HashMap<DefId, Ty> = HashMap::new();
        for param in &params {
            let ty = self.eval_ty(param.ty, &empty);
            if let Some(def) = self.res.item_defs.get(&id) {
                // Params were declared in the fn's own scope; find their
                // defs via the expr they bind. Param defs are not exprs,
                // so look them up by name among defs is fragile; instead
                // resolve.rs mapped every ident use. Bind by span match.
                let _ = def;
            }
            // Param defs: resolve declared them; find by name+span.
            for (i, d) in self.res.defs.iter().enumerate() {
                if d.span == param.name.span {
                    locals.insert(DefId(i as u32), ty.clone());
                    break;
                }
            }
        }
        let ret_ty = ret.map(|r| self.eval_ty(r, &empty));

        for _ in 0..WIDEN_CAP {
            let before = locals.clone();
            for stmt in &body {
                self.type_stmt(*stmt, &mut locals, ret_ty.as_ref());
            }
            if locals == before {
                // Stable: one more pass with errors on.
                self.emit = true;
                for stmt in &body {
                    self.type_stmt(*stmt, &mut locals, ret_ty.as_ref());
                }
                self.emit = false;
                self.types.local_tys.extend(locals);
                return;
            }
        }
        self.emit = true;
        let span = self.ast.item_spans[id.0 as usize].clone();
        self.error(
            span,
            "widths in this body grow without bound; add an explicit `trunc`".to_string(),
        );
        self.emit = false;
    }

    fn type_stmt(&mut self, id: StmtId, locals: &mut HashMap<DefId, Ty>, ret: Option<&Ty>) {
        match self.ast.stmt(id).clone() {
            Stmt::Expr(e) => {
                self.type_expr(e, locals);
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
                self.check_cond(cond, locals);
                for s in then_body {
                    self.type_stmt(s, locals, ret);
                }
                for s in else_body.unwrap_or_default() {
                    self.type_stmt(s, locals, ret);
                }
            }
            Stmt::While { cond, body } => {
                self.check_cond(cond, locals);
                for s in body {
                    self.type_stmt(s, locals, ret);
                }
            }
        }
    }

    fn check_cond(&mut self, cond: ExprId, locals: &mut HashMap<DefId, Ty>) {
        let ty = self.type_expr(cond, locals);
        match ty {
            Ty::Bits(Width::Known(1)) | Ty::Bits(Width::Unknown) | Ty::Unknown | Ty::Int => {}
            other => self.error(
                self.expr_span(cond),
                format!("condition must be bits[1], got {other} (compare explicitly)"),
            ),
        }
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
                    self.check_assignable(&rhs_ty, &state, self.expr_span(rhs), "state write");
                    self.check_literal_fits(rhs, &state);
                } else if self.res.def(def).kind == DefKind::Local {
                    let merged = match locals.get(&def) {
                        Some(old) => self.widen(old.clone(), rhs_ty, lhs),
                        None => rhs_ty,
                    };
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

    /// A constant written into `bits[w]` must fit in `w` bits.
    fn check_literal_fits(&mut self, value: ExprId, target: &Ty) {
        if let Ty::Bits(Width::Known(w)) = target
            && let Some(v) = self.const_eval(value, &HashMap::new())
            && bits_needed(v) > *w
        {
            self.error(
                self.expr_span(value),
                format!("{v} does not fit in bits[{w}]"),
            );
        }
    }

    /// May `value` be written where `target` is expected? Shapes must
    /// match; a known-wider value needs an explicit `trunc`.
    fn check_assignable(&mut self, value: &Ty, target: &Ty, span: Span, what: &str) {
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
                            "{what} would silently truncate bits[{v}] to bits[{t}]; \
                             use `trunc(value, {t})`"
                        ),
                    );
                }
            }
            _ if value == target => {}
            _ => self.error(span, format!("{what}: expected {target}, got {value}")),
        }
    }

    fn type_expr(&mut self, id: ExprId, locals: &mut HashMap<DefId, Ty>) -> Ty {
        let ty = self.type_expr_inner(id, locals);
        self.types.expr_tys.insert(id, ty.clone());
        ty
    }

    fn type_expr_inner(&mut self, id: ExprId, locals: &mut HashMap<DefId, Ty>) -> Ty {
        match self.ast.expr(id).clone() {
            Expr::Int(_) => Ty::Int,
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
                    _ => Ty::Unknown,
                }
            }
            Expr::Unary { operand, .. } => {
                let t = self.type_expr(operand, locals);
                match t {
                    Ty::Bits(_) | Ty::Int | Ty::Unknown => t,
                    other => {
                        self.error(
                            self.expr_span(id),
                            format!("unary operator needs bits, got {other}"),
                        );
                        Ty::Unknown
                    }
                }
            }
            Expr::Binary { op, lhs, rhs } => {
                let l = self.type_expr(lhs, locals);
                let r = self.type_expr(rhs, locals);
                self.type_binop(op, l, r, id)
            }
            Expr::Guard(inner) => self.type_expr(inner, locals),
            Expr::Field { base, .. } => {
                self.type_expr(base, locals);
                // Fields on handles (spawn results) are untyped in v0.
                Ty::Unknown
            }
            Expr::Spawn(inner) => {
                self.type_expr(inner, locals);
                Ty::Unknown
            }
            Expr::Bracket { callee, args } => self.type_bracket(id, callee, &args, locals),
            Expr::Call { callee, args } => self.type_call(id, callee, &args, locals),
        }
    }

    /// Solver-2 width rules. Modular arithmetic: `+`/`-`/bitwise keep the
    /// max width; `*` sums; shifts keep the left width; comparisons give
    /// bits[1]. `Int` absorbs into the other side.
    fn type_binop(&mut self, op: BinOp, l: Ty, r: Ty, at: ExprId) -> Ty {
        use BinOp::*;
        let span = self.expr_span(at);
        if matches!(op, Eq | Ne | Lt | Le | Gt | Ge) {
            return Ty::Bits(Width::Known(1));
        }
        if op == Range {
            return Ty::Unknown; // only meaningful inside any(...)
        }
        match (l, r) {
            (Ty::Unknown, _) | (_, Ty::Unknown) => Ty::Unknown,
            (Ty::Int, Ty::Int) => Ty::Int,
            (Ty::Bits(w), Ty::Int) | (Ty::Int, Ty::Bits(w)) => Ty::Bits(w),
            (Ty::Bits(a), Ty::Bits(b)) => match op {
                Mul => Ty::Bits(match (a, b) {
                    (Width::Known(x), Width::Known(y)) => Width::Known(x + y),
                    _ => Width::Unknown,
                }),
                Shl | Shr => Ty::Bits(a),
                _ => Ty::Bits(match (a, b) {
                    (Width::Known(x), Width::Known(y)) => Width::Known(x.max(y)),
                    _ => Width::Unknown,
                }),
            },
            (l, r) => {
                self.error(
                    span,
                    format!("operator needs bits operands, got {l} and {r}"),
                );
                Ty::Unknown
            }
        }
    }

    /// Brackets: memory read, bit/slice select, or fifo op.
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
            && let Some(Ty::Fifo(elem)) = self.state_tys.get(&def).cloned()
        {
            self.types.expr_tys.insert(callee, Ty::Fifo(elem.clone()));
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
                    self.check_assignable(&arg, &elem, self.expr_span(args[0]), "enqueue");
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
                // Bit select or slice.
                if let Some(arg) = args.first()
                    && let Expr::Binary {
                        op: BinOp::Range,
                        lhs,
                        rhs,
                    } = self.ast.expr(*arg)
                    && let (Some(hi), Some(lo)) = (
                        self.const_eval(*lhs, &HashMap::new()),
                        self.const_eval(*rhs, &HashMap::new()),
                    )
                {
                    return Ty::Bits(Width::Known(hi.abs_diff(lo) + 1));
                }
                let _ = w;
                Ty::Bits(Width::Known(1))
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
    fn type_call(
        &mut self,
        id: ExprId,
        callee: ExprId,
        args: &[ExprId],
        locals: &mut HashMap<DefId, Ty>,
    ) -> Ty {
        let arg_tys: Vec<Ty> = args.iter().map(|a| self.type_expr(*a, locals)).collect();
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
                // Match `bits[N]` params against known arg widths.
                let mut env: HashMap<DefId, u64> = HashMap::new();
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
                // Check each arg against its (instantiated) param type.
                for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
                    let pty = self.eval_ty(param.ty, &env);
                    self.check_assignable(arg_ty, &pty, self.expr_span(*arg), "argument");
                }
                match ret {
                    Some(r) => self.eval_ty(r, &env),
                    None => Ty::Unit,
                }
            }
            DefKind::Rule => {
                self.error(
                    self.expr_span(id),
                    format!(
                        "rules are not callable (`{}` fires on its own)",
                        def_info.name
                    ),
                );
                Ty::Unknown
            }
            DefKind::Reg | DefKind::Mem | DefKind::Fifo | DefKind::Module => {
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

    /// `bits[N]` where `N` is an implicit param -> that param's def.
    fn implicit_width_param(&self, ty: ExprId) -> Option<DefId> {
        let Expr::Bracket { callee, args } = self.ast.expr(ty) else {
            return None;
        };
        if !self.is_builtin(*callee, "bits") {
            return None;
        }
        let def = self.res.expr_defs.get(args.first()?)?;
        (self.res.def(*def).kind == DefKind::ImplicitParam).then_some(*def)
    }

    fn type_builtin_call(&mut self, id: ExprId, name: &str, args: &[ExprId], arg_tys: &[Ty]) -> Ty {
        match name {
            "clog2" | "len" => Ty::Int,
            "trunc" => {
                if args.len() != 2 {
                    self.error(
                        self.expr_span(id),
                        "`trunc` takes (value, width)".to_string(),
                    );
                    return Ty::Unknown;
                }
                match self.const_eval(args[1], &HashMap::new()) {
                    Some(w) => Ty::Bits(Width::Known(w)),
                    None => Ty::Bits(Width::Unknown),
                }
            }
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
            "prio" => match arg_tys.first() {
                Some(Ty::Bits(Width::Known(w))) => Ty::Bits(Width::Known(clog2(*w).max(1))),
                _ => Ty::Bits(Width::Unknown),
            },
            "sync" | "race" => Ty::Unit,
            // any(range) is a model-checker free variable.
            "any" => Ty::Bits(Width::Unknown),
            _ => Ty::Unknown,
        }
    }
}
