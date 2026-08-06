//! The text-splice pass for closures (DESIGN.md's "Closures and partial
//! application"): a `let`-bound local whose init contains `_` is a
//! closure, never a real value (`resolve.rs`'s `closure_inits` already
//! marks which locals these are). This pass erases every one of them
//! from the source before anything else ever sees it — same "interpret
//! down to trace SOURCE text, splice, let the caller re-run the whole
//! frontend" shape `elaborate.rs`'s own doc comment describes for
//! `<elaborates>` list recursion, applied here to a different construct.
//!
//! Two consuming shapes, both pure text substitution, no evaluation:
//! - A bare read (`xs.map(f)`) splices the closure's own body text
//!   VERBATIM, `_` intact — this is what makes `xs.map(f)` and
//!   `xs.map(Add(_, 5))` the same program, letting `elaborate.rs`'s own
//!   `map` builtin (built earlier) handle the result with no awareness
//!   that a closure-local was ever involved.
//! - A call (`f(3)`) splices the closure's body text with each `_`
//!   replaced by that call's own argument text, positionally.
//!
//! Why this can't be eager, ordinary local resolution (`firrtl/calls.rs`'s
//! `self.locals`, which only ever substitutes at a READ site): a closure
//! is not hardware realized at its `let` — evaluating `Add(_, 5)`
//! eagerly, the way types.rs/effects.rs/bounds.rs walk every OTHER
//! `Stmt::Let`'s init immediately, would either choke on the unbound `_`
//! or (worse, since `_` types as `Ty::Unknown` and poisons silently)
//! quietly fold `Add`'s own effects into the enclosing rule even if `f`
//! is never actually called. Deleting the `let` outright and deferring
//! everything to each call site's own splice gives the correct semantics
//! for free: an unused closure contributes nothing, exactly like an
//! unused ordinary function would if it were simply never called.

use crate::ast::{Ast, Expr, ExprId, Item, Name, Stmt, StmtId};
use crate::lexer::Span;
use crate::resolve::{DefId, Resolution};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosureError {
    pub span: Span,
    pub message: String,
}

/// Finds every closure-shaped `let` and every reference to it, reducing
/// each to a source-text edit — same shape as `elaborate::plan`'s own
/// `(Vec<(Span, String)>, Vec<ClosureError>)` return, and its edits are
/// applied the identical way, via `elaborate::render` (fully generic
/// span-replacement, no reason to duplicate it here).
pub fn plan(ast: &Ast, res: &Resolution, src: &str) -> (Vec<(Span, String)>, Vec<ClosureError>) {
    let mut edits = Vec::new();
    let mut errors = Vec::new();
    let mut stack: Vec<_> = ast.roots.clone();
    while let Some(id) = stack.pop() {
        match ast.item(id).clone() {
            Item::Module { items, .. } => stack.extend(items),
            Item::Rule { body, .. } | Item::Fn { body, .. } => {
                scan_stmts(ast, res, src, &body, &mut edits, &mut errors);
            }
            _ => {}
        }
    }
    (edits, errors)
}

fn def_of_name(res: &Resolution, name: &Name) -> Option<DefId> {
    res.defs
        .iter()
        .enumerate()
        .find(|(_, d)| d.span == name.span)
        .map(|(i, _)| DefId(i as u32))
}

fn scan_stmts(
    ast: &Ast,
    res: &Resolution,
    src: &str,
    stmts: &[StmtId],
    edits: &mut Vec<(Span, String)>,
    errors: &mut Vec<ClosureError>,
) {
    for &stmt in stmts {
        match ast.stmt(stmt).clone() {
            Stmt::Let { name, init } => {
                // A closure-shaped `let` is deleted outright, its init
                // never scanned -- it's a template, not code any pass
                // (including this one) should walk as if it runs.
                if def_of_name(res, &name).is_some_and(|d| res.closure_inits.contains_key(&d)) {
                    let span = ast.stmt_spans[stmt.0 as usize].clone();
                    edits.push((span, String::new()));
                } else {
                    scan_expr(ast, res, src, init, edits, errors);
                }
            }
            Stmt::Expr(e) => scan_expr(ast, res, src, e, edits, errors),
            Stmt::Assign { lhs, rhs } => {
                scan_expr(ast, res, src, lhs, edits, errors);
                scan_expr(ast, res, src, rhs, edits, errors);
            }
            Stmt::Return(Some(e)) => scan_expr(ast, res, src, e, edits, errors),
            Stmt::Return(None) | Stmt::Tick | Stmt::Break => {}
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                scan_expr(ast, res, src, cond, edits, errors);
                scan_stmts(ast, res, src, &then_body, edits, errors);
                if let Some(b) = &else_body {
                    scan_stmts(ast, res, src, b, edits, errors);
                }
            }
            Stmt::IfLet {
                init,
                then_body,
                else_body,
                ..
            } => {
                scan_expr(ast, res, src, init, edits, errors);
                scan_stmts(ast, res, src, &then_body, edits, errors);
                if let Some(b) = &else_body {
                    scan_stmts(ast, res, src, b, edits, errors);
                }
            }
            Stmt::While { cond, body } => {
                scan_expr(ast, res, src, cond, edits, errors);
                scan_stmts(ast, res, src, &body, edits, errors);
            }
            Stmt::WhileLet { init, body, .. } => {
                scan_expr(ast, res, src, init, edits, errors);
                scan_stmts(ast, res, src, &body, edits, errors);
            }
        }
    }
}

fn scan_expr(
    ast: &Ast,
    res: &Resolution,
    src: &str,
    id: ExprId,
    edits: &mut Vec<(Span, String)>,
    errors: &mut Vec<ClosureError>,
) {
    let span = ast.expr_spans[id.0 as usize].clone();
    if let Expr::Call { callee, args } = ast.expr(id).clone()
        && let Some(def) = res.expr_defs.get(&callee)
        && let Some(&body) = res.closure_inits.get(def)
    {
        let mut wildcards = Vec::new();
        collect_wildcards(ast, body, &mut wildcards);
        wildcards.sort_by_key(|s| s.start);
        if wildcards.len() != args.len() {
            errors.push(ClosureError {
                span,
                message: format!(
                    "this closure takes {} argument(s) (one per `_`), got {}",
                    wildcards.len(),
                    args.len()
                ),
            });
            return;
        }
        let arg_texts: Vec<String> = args.iter().map(|a| arg_text(ast, src, *a)).collect();
        let substituted = substitute(
            src,
            &ast.expr_spans[body.0 as usize],
            &wildcards,
            &arg_texts,
        );
        edits.push((span, format!("({substituted})")));
        // The call's own arguments are real code (may themselves contain
        // further closure calls/reads), unlike the closure body they're
        // being spliced into -- scan them too.
        for a in args {
            scan_expr(ast, res, src, a, edits, errors);
        }
        return;
    }
    if let Expr::Ident(_) = ast.expr(id)
        && let Some(def) = res.expr_defs.get(&id)
        && let Some(&body) = res.closure_inits.get(def)
    {
        let body_text =
            &src[ast.expr_spans[body.0 as usize].start..ast.expr_spans[body.0 as usize].end];
        edits.push((span, format!("({body_text})")));
        return;
    }
    for child in crate::lower::sub_exprs(ast, id) {
        scan_expr(ast, res, src, child, edits, errors);
    }
}

fn collect_wildcards(ast: &Ast, id: ExprId, out: &mut Vec<Span>) {
    if matches!(ast.expr(id), Expr::Wildcard) {
        out.push(ast.expr_spans[id.0 as usize].clone());
    }
    for child in crate::lower::sub_exprs(ast, id) {
        collect_wildcards(ast, child, out);
    }
}

/// A call argument's own text, parenthesized unless it's atomic --
/// mirrors `elaborate.rs`'s own `ElabValue::Circuit` discipline ("atomic
/// forms... are unparenthesized; anything a surrounding operator could
/// misparse is wrapped in its own parens at construction time"), kept
/// deliberately more conservative here (only a bare identifier or
/// literal counts as atomic, not a call/field/bracket too) rather than
/// risk missing a real precedence case in a fresh implementation.
fn arg_text(ast: &Ast, src: &str, id: ExprId) -> String {
    let span = &ast.expr_spans[id.0 as usize];
    let text = &src[span.start..span.end];
    if matches!(
        ast.expr(id),
        Expr::Ident(_) | Expr::Int(_) | Expr::SizedInt { .. }
    ) {
        text.to_string()
    } else {
        format!("({text})")
    }
}

/// Replaces each of `wildcards` (spans into `src`, already sorted, all
/// falling within `body_span`) with the correspondingly-positioned
/// `arg_texts` entry, returning the substituted text of `body_span`
/// alone -- NOT `elaborate::render` (which replaces spans in the WHOLE
/// `src` and returns the whole file): the result here is itself only a
/// fragment, about to be spliced into a LARGER edit at its own call
/// site's span.
fn substitute(src: &str, body_span: &Span, wildcards: &[Span], arg_texts: &[String]) -> String {
    let mut out = String::new();
    let mut pos = body_span.start;
    for (w, a) in wildcards.iter().zip(arg_texts) {
        out.push_str(&src[pos..w.start]);
        out.push_str(a);
        pos = w.end;
    }
    out.push_str(&src[pos..body_span.end]);
    out
}
