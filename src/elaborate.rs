//! The elaboration-time pass: `<elaborates>` code runs once, at compile
//! time, before any circuit exists (DESIGN.md's "elaboration time"
//! section) — a REAL interpreter over `Stmt`/`Expr` (sequential
//! execution, real `if`/`return` control flow, real list slicing and
//! `len()`), not a splice-and-compile pass like `firrtl::calls`'s
//! ordinary callee inlining.
//!
//! Architecture: this mirrors `lower.rs`, not `firrtl::calls`. An early
//! design spliced freshly-synthesized `Expr` nodes straight into the
//! `Ast` during FIRRTL emission, but `firrtl::Emitter::ast` is a shared
//! `&Ast` for a reason — every synthesized node would need real
//! type/width records, and `types.rs` only ever runs once, before
//! emission, over the ORIGINAL tree. So instead: `plan` finds every
//! `<elaborates>` call reachable from ordinary (non-`<elaborates>`) code,
//! interprets it down to a string of real trace SOURCE syntax (not
//! FIRRTL), and `render` splices that text over the call expression's own
//! span — exactly the text-splice-then-reparse round trip `lower::plan`/
//! `lower::render` already use for `<sequences>` lowering. The caller
//! re-runs the whole frontend (lex/parse/resolve/effects/types) on the
//! spliced source, so the synthesized expressions get ordinary, real type
//! checking for free.
//!
//! A call to another `<elaborates>` function found WHILE interpreting
//! (e.g. `AdderTree`'s own recursion) never becomes a separate splice
//! site: `eval_elab_call_expr` recurses the interpreter directly and
//! folds the result into the same string, so only the OUTERMOST call
//! reachable from ordinary code ever needs an edit.
//!
//! `wire[T]` (DESIGN.md's own type wrapper) has no representation here.
//! Every list ELEMENT is an ordinary `<combines>`-valued expression
//! (never a fifo op, guard, or state-writing call — none of those can
//! appear in elaboration positions at all, per effects.rs's own
//! `check_expr`), read exactly as many times as the source references
//! it — no aliasing/re-instantiation risk `wire` would need to guard
//! against, so v0 elides it entirely (see DESIGN.md's "elaborates"
//! section for the full reasoning).
//!
//! `sig.fails`/`sig.writes` (beyond the single state-write check below)
//! are structurally impossible for an `<elaborates>` function
//! (effects.rs rejects a guard, fifo op, or state write in an
//! elaboration position outright) — this interpreter doesn't re-check
//! either, since a program reaching it already passed effects.rs's
//! checks.
//!
//! Termination is DESIGN.md's own stated gap ("Termination of
//! `elaborates` recursion is unchecked in v0") — but that's a language
//! guarantee gap, not license for THIS interpreter to hang the compiler
//! on a malformed or genuinely non-terminating input: `MAX_DEPTH` is a
//! hard backstop, an explicit error rather than a stack overflow or an
//! infinite loop.

use crate::ast::{Ast, BinOp, Expr, ExprId, Item, ItemId, Param, Stmt, StmtId, UnOp};
use crate::effects::Effects;
use crate::lexer::Span;
use crate::resolve::{DefId, Resolution};

/// A depth this deep is already almost certainly a non-terminating (or
/// just mistakenly unbounded) recursion — DESIGN.md's `AdderTree`-style
/// tree recursion over a real list only ever needs `log2(len)` levels, so
/// 64 is generous for any realistic list, not a tight limit tuned to one
/// example.
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElabError {
    pub span: Span,
    pub message: String,
}

/// What one elaboration-time expression reduces to.
#[derive(Clone)]
enum ElabValue {
    /// A circuit-valued expression, rendered as real trace SOURCE text
    /// (an existing sub-expression's own original text, reused verbatim,
    /// or freshly composed from reduced pieces) — atomic forms (an
    /// identifier, a call) are unparenthesized; anything a surrounding
    /// operator could misparse (a synthesized binary/unary op) is
    /// wrapped in its own parens at construction time.
    Circuit(String),
    /// An elaboration-time integer, computed directly in Rust — a list
    /// length, an index, an arithmetic result. Only turned into source
    /// text (a bare decimal literal) if a `Circuit` position ends up
    /// needing one.
    Int(u64),
    /// A concrete-length list of circuit-valued source-text fragments —
    /// the only place a list's actual length is ever real; `Ty::List`
    /// (types.rs) deliberately never tracks it.
    List(Vec<String>),
}

impl ElabValue {
    fn describe(&self) -> &'static str {
        match self {
            ElabValue::Circuit(_) => "a value",
            ElabValue::Int(_) => "an elaboration-time integer",
            ElabValue::List(_) => "a list",
        }
    }
}

/// Statement-sequence execution outcome: either control fell off the end
/// (`Continue`, only valid if the caller keeps going — a function body
/// reaching this point without a `return` is itself an error the caller
/// reports) or a `return` was reached.
enum ElabFlow {
    Returned(ElabValue),
    Continue,
}

/// Finds every `<elaborates>` call reachable from ordinary code and
/// reduces each to a trace-source-text replacement for its own call-
/// expression span. Errors are collected, not fatal to the whole pass, so
/// one bad call site doesn't hide problems in another.
pub fn plan(
    ast: &Ast,
    res: &Resolution,
    fx: &Effects,
    src: &str,
) -> (Vec<(Span, String)>, Vec<ElabError>) {
    let mut edits = Vec::new();
    let mut errors = Vec::new();
    let mut stack: Vec<ItemId> = ast.roots.clone();
    while let Some(id) = stack.pop() {
        match ast.item(id).clone() {
            Item::Module { items, .. } => stack.extend(items),
            Item::Rule { body, .. } => scan_body(ast, res, fx, src, &body, &mut edits, &mut errors),
            Item::Fn { body, .. } => {
                // A call reachable only from inside another `<elaborates>`
                // function's own body is handled by the interpreter's own
                // recursion (`eval_elab_call_expr`), not by this outer
                // scan — scanning it too would double-evaluate it.
                if fx.sigs.get(&id).is_some_and(|s| s.elaborates) {
                    continue;
                }
                scan_body(ast, res, fx, src, &body, &mut edits, &mut errors);
            }
            _ => {}
        }
    }
    (edits, errors)
}

/// Splices every edit from `plan` over `src`, replacing each call
/// expression's own span with its reduced text — same mechanism as
/// `lower::render`'s text splice, applied earlier in the pipeline.
pub fn render(src: &str, edits: &[(Span, String)]) -> String {
    let mut sorted = edits.to_vec();
    sorted.sort_by_key(|(s, _)| s.start);
    let mut out = String::new();
    let mut pos = 0;
    for (span, text) in &sorted {
        assert!(span.start >= pos, "overlapping elaboration edits");
        out.push_str(&src[pos..span.start]);
        out.push_str(text);
        pos = span.end;
    }
    out.push_str(&src[pos..]);
    out
}

fn scan_body(
    ast: &Ast,
    res: &Resolution,
    fx: &Effects,
    src: &str,
    stmts: &[StmtId],
    edits: &mut Vec<(Span, String)>,
    errors: &mut Vec<ElabError>,
) {
    for stmt in stmts {
        for e in stmt_exprs(ast, *stmt) {
            scan_expr(ast, res, fx, src, e, edits, errors);
        }
        match ast.stmt(*stmt) {
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                scan_body(ast, res, fx, src, then_body, edits, errors);
                if let Some(b) = else_body {
                    scan_body(ast, res, fx, src, b, edits, errors);
                }
            }
            Stmt::While { body, .. } => scan_body(ast, res, fx, src, body, edits, errors),
            _ => {}
        }
    }
}

/// Top-level expressions directly reachable from one statement (mirrors
/// `lower.rs`'s private `stmt_exprs` — not reused across modules since
/// it's a tiny match, same style as the codebase's other small local
/// dupes like `ast.rs`'s `UnOp` symbol match).
fn stmt_exprs(ast: &Ast, id: StmtId) -> Vec<ExprId> {
    match ast.stmt(id).clone() {
        Stmt::Expr(e) => vec![e],
        Stmt::Assign { lhs, rhs } => vec![lhs, rhs],
        Stmt::Let { init, .. } => vec![init],
        Stmt::Tick => vec![],
        Stmt::Return(e) => e.into_iter().collect(),
        Stmt::If { cond, .. } => vec![cond],
        Stmt::While { cond, .. } => vec![cond],
    }
}

fn scan_expr(
    ast: &Ast,
    res: &Resolution,
    fx: &Effects,
    src: &str,
    id: ExprId,
    edits: &mut Vec<(Span, String)>,
    errors: &mut Vec<ElabError>,
) {
    if let Expr::Call { callee, args } = ast.expr(id).clone()
        && is_elaborates_target(res, fx, callee)
    {
        let span = ast.expr_spans[id.0 as usize].clone();
        let mut interp = Interp {
            ast,
            res,
            fx,
            src,
            errors: Vec::new(),
        };
        if let Ok(text) = interp.elaborate_call(span.clone(), callee, &args) {
            edits.push((span, text));
        }
        errors.extend(interp.errors);
        return;
    }
    for child in crate::lower::sub_exprs(ast, id) {
        scan_expr(ast, res, fx, src, child, edits, errors);
    }
}

fn is_elaborates_target(res: &Resolution, fx: &Effects, callee: ExprId) -> bool {
    let Some(def) = res.expr_defs.get(&callee) else {
        return false;
    };
    res.item_defs
        .iter()
        .find(|(_, d)| *d == def)
        .map(|(item, _)| *item)
        .is_some_and(|item| fx.sigs.get(&item).is_some_and(|s| s.elaborates))
}

struct Interp<'a> {
    ast: &'a Ast,
    res: &'a Resolution,
    fx: &'a Effects,
    src: &'a str,
    errors: Vec<ElabError>,
}

impl<'a> Interp<'a> {
    fn error(&mut self, span: Span, message: String) {
        self.errors.push(ElabError { span, message });
    }

    /// The original source text of an existing expression, reused
    /// verbatim — preserves whatever parenthesization the author already
    /// wrote, so a pass-through value never needs its own precedence
    /// analysis.
    fn text_of(&self, id: ExprId) -> String {
        let span = &self.ast.expr_spans[id.0 as usize];
        self.src[span.start..span.end].to_string()
    }

    /// Entry point: `callee`/`args` is a `Call` to a function already
    /// confirmed `sig.elaborates` (by the caller). Evaluates it to
    /// completion and returns the trace-source text the call site should
    /// be replaced with — `args` are evaluated in the CALLER's own
    /// context (no elaboration-time locals bound yet), matching an
    /// ordinary call's own arguments.
    fn elaborate_call(
        &mut self,
        span: Span,
        callee: ExprId,
        args: &[ExprId],
    ) -> Result<String, ()> {
        let arg_vals: Vec<ElabValue> = args
            .iter()
            .map(|a| self.eval_elab_expr(*a, &[], 0))
            .collect::<Result<_, _>>()?;
        let (fn_item, params, body) = self.resolve_elab_target(span.clone(), callee)?;
        let sig = self.fx.sigs.get(&fn_item);
        if sig.is_some_and(|s| !s.writes.is_empty()) {
            self.error(
                span.clone(),
                "an `<elaborates>` function that writes state is not yet supported \
                 (v0 restriction: elaboration-time code may only build a circuit-\
                 valued expression, not write a register/port/memory)"
                    .to_string(),
            );
            return Err(());
        }
        let result = self.eval_elab_fn_body(span.clone(), &params, &body, arg_vals, 1)?;
        self.elab_value_to_text(span, result)
    }

    /// Resolves `callee` to its `Item::Fn` pieces.
    fn resolve_elab_target(
        &mut self,
        span: Span,
        callee: ExprId,
    ) -> Result<(ItemId, Vec<Param>, Vec<StmtId>), ()> {
        let Some(&def) = self.res.expr_defs.get(&callee) else {
            self.error(span, "cannot find this function's item".to_string());
            return Err(());
        };
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
        Ok((fn_item, params, body))
    }

    /// Binds `params` to `arg_vals` in a fresh elaboration-time scope and
    /// executes `body` to completion (real interpretation: sequential
    /// statements, `if`/`return` control flow — a real compile-time
    /// program, unlike `<combines>`, which maps directly to
    /// combinational hardware).
    fn eval_elab_fn_body(
        &mut self,
        span: Span,
        params: &[Param],
        body: &[StmtId],
        arg_vals: Vec<ElabValue>,
        depth: usize,
    ) -> Result<ElabValue, ()> {
        if depth > MAX_DEPTH {
            self.error(
                span,
                format!(
                    "elaboration-time recursion exceeded the depth limit ({MAX_DEPTH}) — \
                     likely a non-terminating `<elaborates>` function (DESIGN.md: \
                     termination of elaborates recursion is unchecked in v0, so this is \
                     the compiler's own backstop, not a language guarantee)"
                ),
            );
            return Err(());
        }
        let mut env: Vec<(DefId, ElabValue)> = Vec::new();
        for (param, val) in params.iter().zip(arg_vals) {
            if let Some(def) = self.def_of_name(&param.name) {
                env.push((def, val));
            }
        }
        match self.eval_elab_stmts(body, &mut env, depth)? {
            ElabFlow::Returned(v) => Ok(v),
            ElabFlow::Continue => {
                self.error(
                    span,
                    "this `<elaborates>` function's body did not return a value on \
                     every path"
                        .to_string(),
                );
                Err(())
            }
        }
    }

    fn def_of_name(&self, name: &crate::ast::Name) -> Option<DefId> {
        self.res
            .defs
            .iter()
            .enumerate()
            .find(|(_, d)| d.span == name.span)
            .map(|(i, _)| DefId(i as u32))
    }

    fn env_get(&self, env: &[(DefId, ElabValue)], def: DefId) -> Option<ElabValue> {
        env.iter()
            .rev()
            .find(|(d, _)| *d == def)
            .map(|(_, v)| v.clone())
    }

    fn eval_elab_stmts(
        &mut self,
        stmts: &[StmtId],
        env: &mut Vec<(DefId, ElabValue)>,
        depth: usize,
    ) -> Result<ElabFlow, ()> {
        for stmt in stmts {
            let span = self.ast.stmt_spans[stmt.0 as usize].clone();
            match self.ast.stmt(*stmt).clone() {
                Stmt::Let { name, init } => {
                    let v = self.eval_elab_expr(init, env, depth)?;
                    if let Some(def) = self.def_of_name(&name) {
                        env.push((def, v));
                    }
                }
                Stmt::Assign { lhs, rhs } => {
                    let Expr::Ident(_) = self.ast.expr(lhs) else {
                        self.error(
                            span,
                            "an `<elaborates>` function may only reassign a plain \
                             local with `:=`, not state or an indexed target (v0 \
                             restriction)"
                                .to_string(),
                        );
                        return Err(());
                    };
                    let Some(&def) = self.res.expr_defs.get(&lhs) else {
                        continue;
                    };
                    if self.res.def(def).kind.is_state() {
                        self.error(
                            span,
                            "an `<elaborates>` function that writes state is not yet \
                             supported (v0 restriction)"
                                .to_string(),
                        );
                        return Err(());
                    }
                    let v = self.eval_elab_expr(rhs, env, depth)?;
                    env.push((def, v));
                }
                Stmt::Return(Some(e)) => {
                    let v = self.eval_elab_expr(e, env, depth)?;
                    return Ok(ElabFlow::Returned(v));
                }
                Stmt::Return(None) => {
                    self.error(
                        span,
                        "an `<elaborates>` function must return a value".to_string(),
                    );
                    return Err(());
                }
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    let c = self.eval_elab_bool(cond, env, depth)?;
                    let branch: &[StmtId] = if c {
                        &then_body
                    } else {
                        else_body.as_deref().unwrap_or(&[])
                    };
                    match self.eval_elab_stmts(branch, env, depth)? {
                        ElabFlow::Returned(v) => return Ok(ElabFlow::Returned(v)),
                        ElabFlow::Continue => {}
                    }
                }
                Stmt::While { .. } => {
                    self.error(
                        span,
                        "a loop in `<elaborates>` code is not yet supported (v0 \
                         restriction: use recursion, DESIGN.md's `AdderTree` style)"
                            .to_string(),
                    );
                    return Err(());
                }
                Stmt::Expr(_) => {
                    self.error(
                        span,
                        "a bare expression statement is not supported in \
                         `<elaborates>` code (v0 restriction: no guards or fifo \
                         ops are legal here at all, and a plain value with no \
                         effect has no purpose as its own statement)"
                            .to_string(),
                    );
                    return Err(());
                }
                Stmt::Tick => {
                    self.error(
                        span,
                        "`tick` is not valid in `<elaborates>` code (elaboration \
                         time has no cycles)"
                            .to_string(),
                    );
                    return Err(());
                }
            }
        }
        Ok(ElabFlow::Continue)
    }

    fn eval_elab_bool(
        &mut self,
        id: ExprId,
        env: &[(DefId, ElabValue)],
        depth: usize,
    ) -> Result<bool, ()> {
        match self.eval_elab_expr(id, env, depth)? {
            ElabValue::Int(v) => Ok(v != 0),
            other => {
                self.error(
                    self.ast.expr_spans[id.0 as usize].clone(),
                    format!(
                        "an `<elaborates>` `if`'s condition must be an elaboration-\
                         time value (e.g. `len(xs) = 1`), got {}",
                        other.describe()
                    ),
                );
                Err(())
            }
        }
    }

    fn eval_elab_int(
        &mut self,
        id: ExprId,
        env: &[(DefId, ElabValue)],
        depth: usize,
    ) -> Result<u64, ()> {
        match self.eval_elab_expr(id, env, depth)? {
            ElabValue::Int(v) => Ok(v),
            other => {
                self.error(
                    self.ast.expr_spans[id.0 as usize].clone(),
                    format!(
                        "expected an elaboration-time integer here, got {}",
                        other.describe()
                    ),
                );
                Err(())
            }
        }
    }

    /// Turns a fully-reduced `ElabValue` into the trace-source text the
    /// call site should be replaced with — an `Int` becomes a plain
    /// decimal literal (so `Sum(...) + 1`-shaped results still work), a
    /// `List` is rejected (a call site always wants a single circuit
    /// value; nothing in this language returns a list from a value
    /// position).
    fn elab_value_to_text(&mut self, span: Span, v: ElabValue) -> Result<String, ()> {
        match v {
            ElabValue::Circuit(s) => Ok(s),
            ElabValue::Int(n) => Ok(n.to_string()),
            ElabValue::List(_) => {
                self.error(
                    span,
                    "an `<elaborates>` function returning a list is not supported \
                     (its call site needs a single circuit value)"
                        .to_string(),
                );
                Err(())
            }
        }
    }

    fn eval_elab_expr(
        &mut self,
        id: ExprId,
        env: &[(DefId, ElabValue)],
        depth: usize,
    ) -> Result<ElabValue, ()> {
        match self.ast.expr(id).clone() {
            Expr::Int(v) => Ok(ElabValue::Int(v)),
            Expr::Ident(_) => {
                if let Some(def) = self.res.expr_defs.get(&id).copied()
                    && let Some(v) = self.env_get(env, def)
                {
                    return Ok(v);
                }
                Ok(ElabValue::Circuit(self.text_of(id)))
            }
            Expr::SizedInt { .. } | Expr::Wildcard => Ok(ElabValue::Circuit(self.text_of(id))),
            Expr::ListLit(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(self.elab_value_to_text_arg(item, env, depth)?);
                }
                Ok(ElabValue::List(out))
            }
            Expr::Unary { op, operand } => {
                let span = self.ast.expr_spans[id.0 as usize].clone();
                match self.eval_elab_expr(operand, env, depth)? {
                    ElabValue::Int(v) => Ok(ElabValue::Int(eval_elab_unop(op, v))),
                    ElabValue::Circuit(e) => {
                        Ok(ElabValue::Circuit(format!("({}{e})", unop_symbol(op))))
                    }
                    other => {
                        self.error(
                            span,
                            format!("cannot apply a unary operator to {}", other.describe()),
                        );
                        Err(())
                    }
                }
            }
            Expr::Binary { op, lhs, rhs } => {
                let span = self.ast.expr_spans[id.0 as usize].clone();
                let l = self.eval_elab_expr(lhs, env, depth)?;
                let r = self.eval_elab_expr(rhs, env, depth)?;
                self.eval_elab_binop(span, op, l, r)
            }
            Expr::Call { callee, args } => {
                let span = self.ast.expr_spans[id.0 as usize].clone();
                self.eval_elab_call_expr(span, callee, &args, env, depth)
            }
            Expr::Bracket { callee, args } => {
                let span = self.ast.expr_spans[id.0 as usize].clone();
                self.eval_elab_bracket(span, callee, &args, env, depth)
            }
            Expr::Field { .. } => {
                // `inst.port` (or similar) — no list/elaboration meaning
                // of its own; pass through as an ordinary circuit
                // reference, same as a plain `Ident` not bound in `env`.
                Ok(ElabValue::Circuit(self.text_of(id)))
            }
            Expr::Guard(_)
            | Expr::Spawn(_)
            | Expr::Range { .. }
            | Expr::Or(_)
            | Expr::StructLit { .. }
            | Expr::OptionTy(_)
            | Expr::Absent => {
                let span = self.ast.expr_spans[id.0 as usize].clone();
                self.error(
                    span,
                    "this expression form has no meaning in `<elaborates>` code".to_string(),
                );
                Err(())
            }
        }
    }

    /// Evaluates `id` and requires a circuit-valued result — used for
    /// list-literal elements, which can never themselves be a nested
    /// list or a bare elaboration-time integer without becoming one
    /// (`Expr::Int` already evaluates as `Int`, folded to a decimal
    /// literal here, so this only ever actually rejects a stray `List`).
    fn elab_value_to_text_arg(
        &mut self,
        id: ExprId,
        env: &[(DefId, ElabValue)],
        depth: usize,
    ) -> Result<String, ()> {
        let span = self.ast.expr_spans[id.0 as usize].clone();
        match self.eval_elab_expr(id, env, depth)? {
            ElabValue::Circuit(s) => Ok(s),
            ElabValue::Int(n) => Ok(n.to_string()),
            ElabValue::List(_) => {
                self.error(
                    span,
                    "a list literal's elements must be values, not nested lists".to_string(),
                );
                Err(())
            }
        }
    }

    fn eval_elab_binop(
        &mut self,
        span: Span,
        op: BinOp,
        l: ElabValue,
        r: ElabValue,
    ) -> Result<ElabValue, ()> {
        if let (ElabValue::Int(a), ElabValue::Int(b)) = (&l, &r) {
            return match eval_elab_int_binop(op, *a, *b) {
                Some(v) => Ok(ElabValue::Int(v)),
                None => {
                    self.error(
                        span,
                        "elaboration-time arithmetic overflowed or divided by zero".to_string(),
                    );
                    Err(())
                }
            };
        }
        let l_text = self.elab_value_as_text(span.clone(), l)?;
        let r_text = self.elab_value_as_text(span.clone(), r)?;
        Ok(ElabValue::Circuit(format!(
            "({l_text} {} {r_text})",
            op.symbol()
        )))
    }

    fn elab_value_as_text(&mut self, span: Span, v: ElabValue) -> Result<String, ()> {
        match v {
            ElabValue::Circuit(s) => Ok(s),
            ElabValue::Int(n) => Ok(n.to_string()),
            ElabValue::List(_) => {
                self.error(span, "cannot use a list as a value here".to_string());
                Err(())
            }
        }
    }

    /// `f(a, b, ...)` inside `<elaborates>` code: `len`/`clog2` compute
    /// directly; a nested call to another `<elaborates>` function
    /// recurses the interpreter; anything else (an ordinary `<combines>`
    /// callee, or one that writes state) is spliced into a fresh
    /// `name(args)` call, its own text built from evaluated arguments,
    /// left for the ordinary frontend to check and the ordinary
    /// `compile_call` path to inline once the whole program is
    /// re-parsed.
    fn eval_elab_call_expr(
        &mut self,
        span: Span,
        callee: ExprId,
        args: &[ExprId],
        env: &[(DefId, ElabValue)],
        depth: usize,
    ) -> Result<ElabValue, ()> {
        if self.is_elab_builtin(callee, "len") {
            let [arg] = args else {
                self.error(span, "`len` takes exactly one argument".to_string());
                return Err(());
            };
            return match self.eval_elab_expr(*arg, env, depth)? {
                ElabValue::List(items) => Ok(ElabValue::Int(items.len() as u64)),
                other => {
                    self.error(
                        span,
                        format!("`len` needs a list, got {}", other.describe()),
                    );
                    Err(())
                }
            };
        }
        if self.is_elab_builtin(callee, "clog2") {
            let [arg] = args else {
                self.error(span, "`clog2` takes exactly one argument".to_string());
                return Err(());
            };
            let v = self.eval_elab_int(*arg, env, depth)?;
            return Ok(ElabValue::Int(clog2(v)));
        }
        let Some(&def) = self.res.expr_defs.get(&callee) else {
            self.error(span, "cannot find this call's target".to_string());
            return Err(());
        };
        if !matches!(
            self.res.def(def).kind,
            crate::resolve::DefKind::Fn | crate::resolve::DefKind::Impl
        ) {
            self.error(
                span,
                "this builtin is not supported in `<elaborates>` code".to_string(),
            );
            return Err(());
        }
        let arg_vals: Vec<ElabValue> = args
            .iter()
            .map(|a| self.eval_elab_expr(*a, env, depth))
            .collect::<Result<_, _>>()?;
        let fn_item = self
            .res
            .item_defs
            .iter()
            .find(|(_, d)| **d == def)
            .map(|(item, _)| *item);
        let is_elab = fn_item.is_some_and(|f| self.fx.sigs.get(&f).is_some_and(|s| s.elaborates));
        if is_elab {
            let (_, params, body) = self.resolve_elab_target(span.clone(), callee)?;
            self.eval_elab_fn_body(span, &params, &body, arg_vals, depth + 1)
        } else {
            let mut arg_texts = Vec::with_capacity(arg_vals.len());
            for v in arg_vals {
                arg_texts.push(self.elab_value_as_text(span.clone(), v)?);
            }
            Ok(ElabValue::Circuit(format!(
                "{}({})",
                self.text_of(callee),
                arg_texts.join(", ")
            )))
        }
    }

    fn is_elab_builtin(&self, id: ExprId, name: &str) -> bool {
        self.res.expr_defs.get(&id).is_some_and(|d| {
            let def = self.res.def(*d);
            def.kind == crate::resolve::DefKind::Builtin && def.name == name
        })
    }

    /// `xs[i]` (an element) or `xs[..mid]`/`xs[mid..]` (a sub-list,
    /// `Expr::Range` — the one-sided form exclusive to list slicing).
    fn eval_elab_bracket(
        &mut self,
        span: Span,
        callee: ExprId,
        args: &[ExprId],
        env: &[(DefId, ElabValue)],
        depth: usize,
    ) -> Result<ElabValue, ()> {
        let base = self.eval_elab_expr(callee, env, depth)?;
        let ElabValue::List(items) = base else {
            self.error(
                span,
                format!(
                    "cannot index {} at elaboration time (only a list can be)",
                    base.describe()
                ),
            );
            return Err(());
        };
        let [arg] = args else {
            self.error(span, "list index/slice takes one argument".to_string());
            return Err(());
        };
        if let Expr::Range { lo, hi } = self.ast.expr(*arg).clone() {
            let lo_v = match lo {
                Some(e) => self.eval_elab_int(e, env, depth)?,
                None => 0,
            };
            let hi_v = match hi {
                Some(e) => self.eval_elab_int(e, env, depth)?,
                None => items.len() as u64,
            };
            if lo_v > hi_v || hi_v > items.len() as u64 {
                self.error(
                    span,
                    format!(
                        "list slice [{lo_v}..{hi_v}] out of bounds for a {}-element list",
                        items.len()
                    ),
                );
                return Err(());
            }
            Ok(ElabValue::List(
                items[lo_v as usize..hi_v as usize].to_vec(),
            ))
        } else {
            let idx = self.eval_elab_int(*arg, env, depth)?;
            match items.get(idx as usize) {
                Some(e) => Ok(ElabValue::Circuit(e.clone())),
                None => {
                    self.error(
                        span,
                        format!(
                            "list index {idx} out of bounds for a {}-element list",
                            items.len()
                        ),
                    );
                    Err(())
                }
            }
        }
    }
}

fn unop_symbol(op: UnOp) -> &'static str {
    match op {
        UnOp::Neg => "-",
        UnOp::Not => "not",
        UnOp::BitNot => "~",
    }
}

fn eval_elab_unop(op: UnOp, v: u64) -> u64 {
    match op {
        UnOp::Neg => v.wrapping_neg(),
        UnOp::Not => (v == 0) as u64,
        UnOp::BitNot => !v,
    }
}

fn eval_elab_int_binop(op: BinOp, l: u64, r: u64) -> Option<u64> {
    match op {
        BinOp::Add => l.checked_add(r),
        BinOp::Sub => l.checked_sub(r),
        BinOp::Mul => l.checked_mul(r),
        BinOp::Div => l.checked_div(r),
        BinOp::Rem => l.checked_rem(r),
        BinOp::Shl => l.checked_shl(r as u32),
        BinOp::Shr => l.checked_shr(r as u32),
        BinOp::BitAnd => Some(l & r),
        BinOp::BitOr => Some(l | r),
        BinOp::BitXor => Some(l ^ r),
        BinOp::Eq => Some((l == r) as u64),
        BinOp::Ne => Some((l != r) as u64),
        BinOp::Lt => Some((l < r) as u64),
        BinOp::Le => Some((l <= r) as u64),
        BinOp::Gt => Some((l > r) as u64),
        BinOp::Ge => Some((l >= r) as u64),
        // An elaboration-time integer has no fixed bit-width, so there's
        // no "sign bit" position to shift relative to — arithmetic shift
        // genuinely can't be folded here, unlike `Shl`/`Shr`.
        BinOp::AShr => None,
        BinOp::Range | BinOp::PlusColon | BinOp::MinusColon => None,
    }
}

/// Mirrors `lower.rs`'s private `clog2` — ceiling log2, `clog2(1) == 0`.
fn clog2(v: u64) -> u64 {
    if v <= 1 {
        0
    } else {
        64 - (v - 1).leading_zeros() as u64
    }
}
