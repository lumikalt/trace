//! `sequences` lowering: DESIGN.md's resolution of the central open
//! question. A `sequences` rule body is sugar. `tick` cuts it into
//! segments; each segment becomes an ordinary single-cycle rule guarded
//! on a continuation register. There is no cross-cycle rollback and no
//! new runtime mechanism — only the primitives already decided (guarded
//! atomic rules, failure = retry).
//!
//! The internal representation IS trace source text, per the doc
//! ("shown as source; the real lowering is internal"): `render` splices
//! generated text over the original rule's span, and the result is meant
//! to be re-run through the whole pipeline (resolve/effects/types/
//! schedule) — that is both the validation strategy and, eventually, the
//! next compiler stage's actual input.
//!
//! Values that cross a tick get a save register. A save register is only
//! sound when the local is a **single assignment, read only in later
//! segments**: promoting it to a register changes same-cycle
//! read-after-write from "sees the new value" (a local/wire) to "sees
//! the old value" (a register is speculative until the clock edge, the
//! whole point of the design). A local reassigned across segments, or
//! read in its own assignment segment, is rejected rather than silently
//! miscompiled.
//!
//! v0 scope, each an explicit error rather than a silent skip:
//! - `tick` must be at the top level of the rule body, not nested in
//!   `if`/`while` (a conditional cycle boundary is real scheduler work,
//!   not yet designed).
//! - `spawn`/`sync`/`race` are not handled by this pass.
//! - a captured local's type must be a concrete `bits[w]`.

use crate::ast::{Ast, Expr, ExprId, Item, ItemId, Stmt, StmtId};
use crate::effects::Effects;
use crate::lexer::Span;
use crate::resolve::{DefId, DefKind, Resolution};
use crate::types::{Ty, Types, Width};
use std::collections::BTreeSet;

#[derive(Debug, Clone)]
pub struct Segment {
    pub index: u64,
    /// Statements belonging to this segment, contiguous, `Tick` excluded.
    pub stmts: Vec<StmtId>,
}

#[derive(Debug, Clone)]
pub struct CapturedLocal {
    pub def: DefId,
    pub name: String,
    pub ty: Ty,
    pub assign_segment: usize,
    pub read_segments: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct LoweredRule {
    pub rule: ItemId,
    pub rule_name: String,
    pub cont_name: String,
    pub cont_width: u64,
    pub segments: Vec<Segment>,
    pub captures: Vec<CapturedLocal>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LowerError {
    pub span: Span,
    pub message: String,
}

fn clog2(v: u64) -> u64 {
    if v <= 1 {
        0
    } else {
        64 - (v - 1).leading_zeros() as u64
    }
}

/// Find every `<sequences>` rule with at least one top-level `tick` and
/// plan its lowering. Rules that fail a v0-scope check are reported and
/// omitted from the result; other rules still lower independently.
pub fn plan(
    ast: &Ast,
    res: &Resolution,
    fx: &Effects,
    types: &Types,
) -> (Vec<LoweredRule>, Vec<LowerError>) {
    let mut out = Vec::new();
    let mut errors = Vec::new();
    let mut stack: Vec<ItemId> = ast.roots.clone();
    while let Some(id) = stack.pop() {
        match ast.item(id).clone() {
            Item::Module { items, .. } => stack.extend(items),
            Item::Rule { name, body, .. } => {
                if !fx.sigs.get(&id).is_some_and(|s| s.sequences) {
                    continue;
                }
                if !body.iter().any(|s| matches!(ast.stmt(*s), Stmt::Tick)) {
                    continue; // nothing to cut; leave as an ordinary rule
                }
                match plan_rule(ast, res, types, id, &name.text, &body) {
                    Ok(lowered) => out.push(lowered),
                    Err(errs) => errors.extend(errs),
                }
            }
            _ => {}
        }
    }
    (out, errors)
}

fn plan_rule(
    ast: &Ast,
    res: &Resolution,
    types: &Types,
    rule: ItemId,
    rule_name: &str,
    body: &[StmtId],
) -> Result<LoweredRule, Vec<LowerError>> {
    let mut errors = Vec::new();

    if let Some(span) = find_nested_tick(ast, body) {
        errors.push(LowerError {
            span,
            message: "`tick` must be at the top level of a sequences rule, not nested in \
                      if/while (v0 restriction)"
                .to_string(),
        });
    }
    if let Some(span) = find_unsupported_construct(ast, res, body) {
        errors.push(LowerError {
            span,
            message: "sequences lowering does not yet support spawn/sync/race (v0 restriction)"
                .to_string(),
        });
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    // Split at top-level ticks.
    let mut segments: Vec<Segment> = vec![Segment {
        index: 0,
        stmts: Vec::new(),
    }];
    for stmt in body {
        if matches!(ast.stmt(*stmt), Stmt::Tick) {
            let next = segments.len() as u64;
            segments.push(Segment {
                index: next,
                stmts: Vec::new(),
            });
        } else {
            segments.last_mut().unwrap().stmts.push(*stmt);
        }
    }

    // Per-local def/use segments, across the whole rule.
    let mut assigns: std::collections::BTreeMap<DefId, BTreeSet<usize>> = Default::default();
    let mut reads: std::collections::BTreeMap<DefId, BTreeSet<usize>> = Default::default();
    for seg in &segments {
        scan_stmts(
            ast,
            res,
            &seg.stmts,
            seg.index as usize,
            &mut assigns,
            &mut reads,
        );
    }

    let mut captures = Vec::new();
    for (def, assign_segs) in &assigns {
        let read_segs = reads.get(def).cloned().unwrap_or_default();
        let touched: BTreeSet<usize> = assign_segs.union(&read_segs).copied().collect();
        if touched.len() <= 1 {
            continue; // stays a plain local within one generated segment
        }
        let name = res.def(*def).name.clone();
        if assign_segs.len() != 1 {
            let segs: Vec<String> = assign_segs.iter().map(|s| s.to_string()).collect();
            errors.push(LowerError {
                span: res.def(*def).span.clone(),
                message: format!(
                    "`{name}` is assigned in multiple segments ({}); sequences lowering \
                     requires a single assignment per captured value (v0 restriction)",
                    segs.join(", ")
                ),
            });
            continue;
        }
        let assign_segment = *assign_segs.iter().next().unwrap();
        if let Some(&bad) = read_segs.iter().find(|&&s| s <= assign_segment) {
            errors.push(LowerError {
                span: res.def(*def).span.clone(),
                message: format!(
                    "`{name}` is read in segment {bad} at or before its assignment in \
                     segment {assign_segment}; a captured value must be write-once and \
                     read only in later segments (v0 restriction: promoting it to a \
                     register would change same-cycle read-after-write semantics)"
                ),
            });
            continue;
        }
        let ty = types.local_tys.get(def).cloned().unwrap_or(Ty::Unknown);
        if !matches!(ty, Ty::Bits(Width::Known(_))) {
            errors.push(LowerError {
                span: res.def(*def).span.clone(),
                message: format!(
                    "`{name}` has type {ty}, not a concrete `bits[w]`; sequences lowering \
                     needs a known width to declare its save register"
                ),
            });
            continue;
        }
        captures.push(CapturedLocal {
            def: *def,
            name,
            ty,
            assign_segment,
            read_segments: read_segs.into_iter().collect(),
        });
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    let cont_width = clog2(segments.len() as u64).max(1);
    Ok(LoweredRule {
        rule,
        rule_name: rule_name.to_string(),
        cont_name: format!("__cont_{rule_name}"),
        cont_width,
        segments,
        captures,
    })
}

/// Called on the rule's top-level body: top-level ticks are fine, only
/// descends into if/while to reject ticks hiding inside them.
fn find_nested_tick(ast: &Ast, stmts: &[StmtId]) -> Option<Span> {
    for stmt in stmts {
        match ast.stmt(*stmt) {
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                if let Some(span) = find_tick_anywhere(ast, then_body) {
                    return Some(span);
                }
                if let Some(span) = else_body
                    .as_deref()
                    .and_then(|b| find_tick_anywhere(ast, b))
                {
                    return Some(span);
                }
            }
            Stmt::While { body, .. } => {
                if let Some(span) = find_tick_anywhere(ast, body) {
                    return Some(span);
                }
            }
            _ => {}
        }
    }
    None
}

fn find_tick_anywhere(ast: &Ast, stmts: &[StmtId]) -> Option<Span> {
    for stmt in stmts {
        match ast.stmt(*stmt) {
            Stmt::Tick => return Some(ast.stmt_spans[stmt.0 as usize].clone()),
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                if let Some(span) = find_tick_anywhere(ast, then_body) {
                    return Some(span);
                }
                if let Some(span) = else_body
                    .as_deref()
                    .and_then(|b| find_tick_anywhere(ast, b))
                {
                    return Some(span);
                }
            }
            Stmt::While { body, .. } => {
                if let Some(span) = find_tick_anywhere(ast, body) {
                    return Some(span);
                }
            }
            _ => {}
        }
    }
    None
}

fn find_unsupported_construct(ast: &Ast, res: &Resolution, stmts: &[StmtId]) -> Option<Span> {
    for stmt in stmts {
        let exprs = stmt_exprs(ast, *stmt);
        for e in exprs {
            if let Some(span) = find_unsupported_in_expr(ast, res, e) {
                return Some(span);
            }
        }
        let nested = match ast.stmt(*stmt) {
            Stmt::If {
                then_body,
                else_body,
                ..
            } => find_unsupported_construct(ast, res, then_body).or_else(|| {
                else_body
                    .as_deref()
                    .and_then(|b| find_unsupported_construct(ast, res, b))
            }),
            Stmt::While { body, .. } => find_unsupported_construct(ast, res, body),
            _ => None,
        };
        if nested.is_some() {
            return nested;
        }
    }
    None
}

fn find_unsupported_in_expr(ast: &Ast, res: &Resolution, id: ExprId) -> Option<Span> {
    match ast.expr(id) {
        Expr::Spawn(_) => return Some(ast.expr_spans[id.0 as usize].clone()),
        Expr::Call { callee, .. } => {
            if let Some(def) = res.expr_defs.get(callee) {
                let d = res.def(*def);
                if d.kind == DefKind::Builtin && (d.name == "sync" || d.name == "race") {
                    return Some(ast.expr_spans[id.0 as usize].clone());
                }
            }
        }
        _ => {}
    }
    for child in sub_exprs(ast, id) {
        if let Some(span) = find_unsupported_in_expr(ast, res, child) {
            return Some(span);
        }
    }
    None
}

/// Direct expression children the parser can produce, one level.
pub(crate) fn sub_exprs(ast: &Ast, id: ExprId) -> Vec<ExprId> {
    match ast.expr(id).clone() {
        Expr::Ident(_) | Expr::Int(_) | Expr::SizedInt { .. } | Expr::Wildcard => vec![],
        Expr::Unary { operand, .. } => vec![operand],
        Expr::Binary { lhs, rhs, .. } => vec![lhs, rhs],
        Expr::Guard(inner) | Expr::Spawn(inner) => vec![inner],
        Expr::Field { base, .. } => vec![base],
        Expr::Call { callee, args } | Expr::Bracket { callee, args } => {
            let mut v = vec![callee];
            v.extend(args);
            v
        }
    }
}

/// Top-level expressions directly reachable from one statement (not
/// recursing into nested statement bodies; the caller handles that).
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

fn scan_stmts(
    ast: &Ast,
    res: &Resolution,
    stmts: &[StmtId],
    segment: usize,
    assigns: &mut std::collections::BTreeMap<DefId, BTreeSet<usize>>,
    reads: &mut std::collections::BTreeMap<DefId, BTreeSet<usize>>,
) {
    for stmt in stmts {
        match ast.stmt(*stmt).clone() {
            Stmt::Assign { lhs, rhs } => {
                scan_expr(ast, res, rhs, segment, reads);
                match ast.expr(lhs) {
                    Expr::Ident(_) => {
                        if let Some(def) = res.expr_defs.get(&lhs)
                            && res.def(*def).kind == DefKind::Local
                        {
                            assigns.entry(*def).or_default().insert(segment);
                        }
                    }
                    _ => scan_expr(ast, res, lhs, segment, reads),
                }
            }
            Stmt::Let { name, init } => {
                scan_expr(ast, res, init, segment, reads);
                if let Some((idx, _)) = res
                    .defs
                    .iter()
                    .enumerate()
                    .find(|(_, d)| d.span == name.span)
                {
                    assigns
                        .entry(DefId(idx as u32))
                        .or_default()
                        .insert(segment);
                }
            }
            Stmt::Expr(e) => scan_expr(ast, res, e, segment, reads),
            Stmt::Tick => {}
            Stmt::Return(Some(e)) => scan_expr(ast, res, e, segment, reads),
            Stmt::Return(None) => {}
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                scan_expr(ast, res, cond, segment, reads);
                scan_stmts(ast, res, &then_body, segment, assigns, reads);
                if let Some(else_body) = else_body {
                    scan_stmts(ast, res, &else_body, segment, assigns, reads);
                }
            }
            Stmt::While { cond, body } => {
                scan_expr(ast, res, cond, segment, reads);
                scan_stmts(ast, res, &body, segment, assigns, reads);
            }
        }
    }
}

fn scan_expr(
    ast: &Ast,
    res: &Resolution,
    id: ExprId,
    segment: usize,
    reads: &mut std::collections::BTreeMap<DefId, BTreeSet<usize>>,
) {
    if let Expr::Ident(_) = ast.expr(id)
        && let Some(def) = res.expr_defs.get(&id)
        && res.def(*def).kind == DefKind::Local
    {
        reads.entry(*def).or_default().insert(segment);
    }
    for child in sub_exprs(ast, id) {
        scan_expr(ast, res, child, segment, reads);
    }
}

/// Render every planned lowering as a text splice over `src`: each
/// original rule's span is replaced by its save registers, continuation
/// register, and segment rules. Everything else in the file is untouched.
///
/// A `schedule` directive that names a lowered rule is regenerated too:
/// `urgency step > refill` becomes `urgency step_s0 > step_s1 > ... >
/// step_sN > refill` (every segment ranks above what `step` ranked
/// above), and `mutually_exclusive { step, x }`/`conflict_free { step,
/// x }` become `mutually_exclusive`/`conflict_free { step_s0, ...,
/// step_sN, x }`. This is regenerated from the parsed directives, not
/// string-matched, so a name like `step2` is never confused with `step`.
pub fn render(ast: &Ast, src: &str, lowered: &[LoweredRule]) -> String {
    let mut edits: Vec<(Span, String)> = lowered
        .iter()
        .map(|lr| {
            let span = ast.item_spans[lr.rule.0 as usize].clone();
            (span, render_rule(ast, src, lr))
        })
        .collect();

    let expansions: std::collections::HashMap<&str, Vec<String>> = lowered
        .iter()
        .map(|lr| {
            let segs = (0..lr.segments.len() as u64)
                .map(|i| format!("{}_s{}", lr.rule_name, i))
                .collect();
            (lr.rule_name.as_str(), segs)
        })
        .collect();
    if !expansions.is_empty() {
        edits.extend(rewrite_schedules(ast, &expansions));
    }

    edits.sort_by_key(|(s, _)| s.start);
    let mut out = String::new();
    let mut pos = 0;
    for (span, text) in &edits {
        assert!(span.start >= pos, "overlapping lowering edits");
        out.push_str(&src[pos..span.start]);
        out.push_str(text);
        pos = span.end;
    }
    out.push_str(&src[pos..]);
    out
}

fn rewrite_schedules(
    ast: &Ast,
    expansions: &std::collections::HashMap<&str, Vec<String>>,
) -> Vec<(Span, String)> {
    let mut edits = Vec::new();
    let mut stack: Vec<ItemId> = ast.roots.clone();
    while let Some(id) = stack.pop() {
        match ast.item(id) {
            Item::Module { items, .. } => stack.extend(items.iter().copied()),
            Item::Schedule { directives } => {
                let touches_lowered = directives.iter().any(|d| {
                    let names = match d {
                        crate::ast::ScheduleDirective::Urgency(ns)
                        | crate::ast::ScheduleDirective::MutuallyExclusive(ns)
                        | crate::ast::ScheduleDirective::ConflictFree(ns) => ns,
                    };
                    names
                        .iter()
                        .any(|n| expansions.contains_key(n.text.as_str()))
                });
                if !touches_lowered {
                    continue;
                }
                let mut out = String::from("schedule {\n");
                for directive in directives {
                    match directive {
                        crate::ast::ScheduleDirective::Urgency(names) => {
                            let flat = expand(names, expansions);
                            out.push_str(&format!("    urgency {}\n", flat.join(" > ")));
                        }
                        crate::ast::ScheduleDirective::MutuallyExclusive(names) => {
                            let flat = expand(names, expansions);
                            out.push_str(&format!(
                                "    mutually_exclusive {{ {} }}\n",
                                flat.join(", ")
                            ));
                        }
                        crate::ast::ScheduleDirective::ConflictFree(names) => {
                            let flat = expand(names, expansions);
                            out.push_str(&format!("    conflict_free {{ {} }}\n", flat.join(", ")));
                        }
                    }
                }
                out.push_str("}\n");
                edits.push((ast.item_spans[id.0 as usize].clone(), out));
            }
            _ => {}
        }
    }
    edits
}

fn expand(
    names: &[crate::ast::Name],
    expansions: &std::collections::HashMap<&str, Vec<String>>,
) -> Vec<String> {
    names
        .iter()
        .flat_map(|n| match expansions.get(n.text.as_str()) {
            Some(segs) => segs.clone(),
            None => vec![n.text.clone()],
        })
        .collect()
}

fn render_rule(ast: &Ast, src: &str, lr: &LoweredRule) -> String {
    let mut out = String::new();
    for cap in &lr.captures {
        out.push_str(&format!("reg {} : {} = 0\n", cap.name, cap.ty));
    }
    out.push_str(&format!(
        "reg {} : bits[{}] = 0\n\n",
        lr.cont_name, lr.cont_width
    ));

    let nsegs = lr.segments.len() as u64;
    for seg in &lr.segments {
        out.push_str(&format!("rule {}_s{} {{\n", lr.rule_name, seg.index));
        out.push_str(&format!("    ({} == {})?\n", lr.cont_name, seg.index));
        for stmt in &seg.stmts {
            let span = ast.stmt_spans[stmt.0 as usize].clone();
            out.push_str(&src[span]);
        }
        let next = if seg.index + 1 < nsegs {
            seg.index + 1
        } else {
            0
        };
        out.push_str(&format!("    {} := {next}\n", lr.cont_name));
        out.push_str("}\n\n");
    }
    out
}
