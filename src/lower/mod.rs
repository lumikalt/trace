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
//! `spawn`/`sync` (2026-08-01, DESIGN.md's fork-resolved algorithm):
//! `h := spawn Callee(args)` at a rule's top level starts an independent
//! FSM, macro-expanded from `Callee`'s own `<sequences>` body using this
//! same segment/capture machinery, scoped to fresh per-occurrence
//! registers (`__cont_{rule}_{h}`, `__done_{rule}_{h}`,
//! `__result_{rule}_{h}`, one `__arg_{rule}_{h}_{param}` per parameter,
//! one `__save_{rule}_{h}_{local}` per callee-internal captured local —
//! every name prefixed by both the enclosing rule and the handle to keep
//! two spawns of the same callee, or the same handle name in two rules,
//! from colliding). A callee's own segment-0 rule COULD in principle fire
//! from a zeroed reset before ever being triggered, or race the trigger
//! itself the cycle it's (re)triggered — checked by hand-simulating a
//! gated-trigger variant before this shipped, and both turn out to be
//! structurally unobservable: `render_rule` always emits the trigger's
//! own segment rules before a spawn's callee segments (so derived-stall
//! priority always favors the trigger on any shared cycle), the trigger
//! itself resets `done` to 0 whenever it fires, and `sync` can never
//! observe `done` before the trigger's own segment has run at least once
//! — so the continuation register resets to a plain 0, same as every
//! other continuation register in this emitter, no sentinel value
//! needed. `sync[h1, h2, ...]` (brackets: sync is a fallible operation,
//! same convention as `f.Deq[]`/`f.Enq[x]`) lowers to one
//! `(__done_h{i} = 1)?` guard per handle, inserted in place of the
//! call — reusing "guard fails, segment retries next cycle" unchanged,
//! no new segmentation trigger. `h.result`/`h.done` anywhere in the
//! enclosing rule rewrite to `__result_*`/`__done_*` reads.
//! `tick` optionally takes a trailing fallible expression
//! (`tick sync[h1, h2]`): parser.rs desugars this into a plain `tick`
//! followed by that expression as the opened segment's own leading
//! statement, so this module never sees a distinct AST shape for it —
//! `tick sync[h1, h2]` and separately writing `tick` then `sync[h1, h2]`
//! on the next line produce identical trees.
//! `race` stays unimplemented (DESIGN.md: needs a loser-cancellation
//! latch not yet designed).
//!
//! v0 scope, each an explicit error rather than a silent skip:
//! - `tick` must be at the top level of the rule body, not nested in
//!   `if`/`while` (a conditional cycle boundary is real scheduler work,
//!   not yet designed). `spawn` has the identical restriction.
//! - `race` is not handled by this pass.
//! - a captured local's type must be a concrete `[w]`.
//! - a spawn callee's `return` must be the last statement of its last
//!   segment; no early return.

use crate::ast::{Ast, Expr, ExprId, Item, ItemId, Stmt, StmtId};
use crate::effects::Effects;
use crate::lexer::Span;
use crate::resolve::{DefId, Resolution};
use crate::types::{Ty, Types};
use std::collections::HashMap;

// Split by responsibility, not by size: `plan` (phase 1 — analyze a rule's
// AST, build its `LoweredRule`/`SpawnPlan`) and `render` (phase 2 —
// splice generated trace source text over the original spans). Each
// file's own doc comment says more. This file keeps the struct/type
// definitions, the two entry points, and the low-level AST-walking
// helpers genuinely shared crate-wide (`sub_exprs`/`guard_chain_spine`,
// used well beyond this pass — see their own doc comments) or between
// both phases here (`find_tick_anywhere`/`find_while_anywhere`, called
// directly by `plan`'s own top-level gate as well as from within
// plan.rs's `find_nested_tick`/`find_nested_while`; `find_break_
// anywhere`, called by that same gate as well as from render.rs's
// `render_loop_body`).
mod plan;
mod render;

use plan::plan_rule;
use render::{render_rule, rewrite_schedules};

#[derive(Debug, Clone)]
pub struct Segment {
    pub index: u64,
    /// Statements belonging to this segment, contiguous, `Tick` excluded.
    /// For a `while`/`while let` loop segment (`is_while_loop` is
    /// `true`), this is exactly one statement: the `Stmt::While`/`Stmt::
    /// WhileLet` itself, unwrapped only at render time (`render_rule`/
    /// `render_spawn_segments`) — kept intact rather than flattened to
    /// its own body so every existing recursive scan (`find_returns`,
    /// capture tracking, ...) that already handles `Stmt::While`/`Stmt::
    /// WhileLet` correctly (rejecting an early `return`, requiring a
    /// concrete width to capture, etc.) keeps working unchanged.
    pub stmts: Vec<StmtId>,
    /// `true` marks this segment as a `while COND { ... }`/`while let
    /// NAME = EXPR { ... }` loop's own self-looping segment (one
    /// iteration per cycle): render as `if COND { <body>; cont := SELF }
    /// else { cont := NEXT }` (plain `while`) or `if let NAME = EXPR {
    /// <body>; cont := SELF } else { cont := NEXT }` (`while let`)
    /// instead of straight-line-then-advance. `false` for an ordinary
    /// tick-cut segment. Just a marker, not `Option<ExprId>` — render
    /// time re-derives `cond`/`name`/`init`/`body` by matching `ast.
    /// stmt(stmts[0])` directly, since which shape applies depends on
    /// which of the two statements it is anyway.
    pub is_while_loop: bool,
}

#[derive(Debug, Clone)]
pub struct CapturedLocal {
    pub def: DefId,
    pub name: String,
    pub ty: Ty,
    pub assign_segment: usize,
    pub read_segments: Vec<usize>,
    /// `Some(span)` if this capture's declaring statement was `let name =
    /// init` — `span` covers exactly `let name = ` (this statement's own
    /// start through `init`'s own start), the prefix a renderer must
    /// replace with `{target_name} := ` before splicing the rest of the
    /// statement verbatim. `None` for a `:=`-declared capture, whose
    /// original text already parses as a register write once its own
    /// backing `reg`/renamed register exists, needing no rewrite.
    pub let_prefix_span: Option<Span>,
}

/// One `spawn`'s callee parameter: a save register written once, at
/// trigger time, from the caller's own argument expression.
#[derive(Debug, Clone)]
pub struct SpawnArg {
    pub reg_name: String,
    pub ty: Ty,
    pub caller_expr: ExprId,
}

/// One `h := spawn Callee(args)` occurrence, fully planned: the callee's
/// own body cut into segments exactly like a top-level rule, renamed
/// into this occurrence's own private registers.
#[derive(Debug, Clone)]
pub struct SpawnPlan {
    pub handle_def: DefId,
    pub handle_name: String,
    pub trigger_stmt: StmtId,
    pub cont_name: String,
    pub cont_width: u64,
    pub done_name: String,
    pub result_name: String,
    pub result_ty: Ty,
    pub args: Vec<SpawnArg>,
    pub segments: Vec<Segment>,
    pub captures: Vec<CapturedLocal>,
    /// Every reference (inside `segments`) to a param or a captured local
    /// rewrites to this register name instead of its original text.
    pub renames: HashMap<DefId, String>,
    /// Pre-computed `(span, replacement)` edits for every renamed
    /// reference anywhere in the callee's body (all segments, including
    /// the final `return`'s own value expression) — render time only
    /// splices, it never needs `Resolution` again.
    pub rename_edits: Vec<(Span, String)>,
    pub return_expr: ExprId,
    pub base_rule_name: String,
}

#[derive(Debug, Clone)]
pub struct LoweredRule {
    pub rule: ItemId,
    pub rule_name: String,
    pub cont_name: String,
    pub cont_width: u64,
    pub segments: Vec<Segment>,
    pub captures: Vec<CapturedLocal>,
    pub spawns: Vec<SpawnPlan>,
    /// `sync[...]` bracket-call statements, each naming the handles it
    /// joins.
    pub syncs: Vec<(StmtId, Vec<DefId>)>,
    /// `race[...]` bracket-call statements, each naming the handles
    /// racing — every handle named in a group is that group's every
    /// OTHER named handle's "competitor" (see `render_rule`'s own doc
    /// comment on cancellation).
    pub races: Vec<(StmtId, Vec<DefId>)>,
    /// Every `h.result`/`h.done` reference anywhere in this rule's own
    /// segments, rewritten to the owning spawn's register name.
    pub handle_field_rewrites: Vec<(Span, String)>,
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
    let def_items: HashMap<DefId, ItemId> = res.item_defs.iter().map(|(i, d)| (*d, *i)).collect();
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
                // A `tick`/`while`/`break` ANYWHERE (not just top-level)
                // has to route through `plan_rule`, even one that turns
                // out to be nested/misplaced (and therefore rejected) —
                // `find_nested_tick`/`find_nested_while`/`find_break_
                // misplaced`'s own clear errors live there, and skipping
                // straight past this gate would leave a nested/misplaced
                // one to fail some other, more confusing way (or, for a
                // stray `break` with no `tick`/`while` alongside it,
                // silently compile away to nothing) once the untouched
                // `<sequences>` rule reaches a later pass instead.
                if find_tick_anywhere(ast, &body).is_none()
                    && find_while_anywhere(ast, &body).is_none()
                    && find_break_anywhere(ast, &body).is_none()
                {
                    continue; // nothing to cut; leave as an ordinary rule
                }
                match plan_rule(ast, res, types, &def_items, id, &name.text, &body) {
                    Ok(lowered) => out.push(lowered),
                    Err(errs) => errors.extend(errs),
                }
            }
            _ => {}
        }
    }
    (out, errors)
}

/// A `<sequences>` body's own checked cycle count, when it has one --
/// `None` for a data-dependent duration (a `while`/`while let` loop, or
/// a `spawn`'d callee waited on via `sync`/`race`). See `plan::sequences_
/// cycle_count`'s own doc comment for the full reasoning; this is just
/// the crate-visible entry point, mirroring `plan`/`render` above.
pub fn sequences_cycle_count(ast: &Ast, body: &[StmtId]) -> Option<u32> {
    plan::sequences_cycle_count(ast, body)
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
            Stmt::IfLet {
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
            Stmt::WhileLet { body, .. } => {
                if let Some(span) = find_tick_anywhere(ast, body) {
                    return Some(span);
                }
            }
            _ => {}
        }
    }
    None
}

fn find_while_anywhere(ast: &Ast, stmts: &[StmtId]) -> Option<Span> {
    for stmt in stmts {
        match ast.stmt(*stmt) {
            Stmt::While { .. } | Stmt::WhileLet { .. } => {
                return Some(ast.stmt_spans[stmt.0 as usize].clone());
            }
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                if let Some(span) = find_while_anywhere(ast, then_body) {
                    return Some(span);
                }
                if let Some(span) = else_body
                    .as_deref()
                    .and_then(|b| find_while_anywhere(ast, b))
                {
                    return Some(span);
                }
            }
            Stmt::IfLet {
                then_body,
                else_body,
                ..
            } => {
                if let Some(span) = find_while_anywhere(ast, then_body) {
                    return Some(span);
                }
                if let Some(span) = else_body
                    .as_deref()
                    .and_then(|b| find_while_anywhere(ast, b))
                {
                    return Some(span);
                }
            }
            _ => {}
        }
    }
    None
}

/// True iff `break` appears ANYWHERE in `stmts`, any nesting depth --
/// used by `plan()`'s own top-level gate (mirroring `find_tick_
/// anywhere`/`find_while_anywhere`'s identical role) so a rule with a
/// stray `break` but no `tick`/`while` anywhere ALSO routes through
/// `plan_rule` (and therefore `find_break_misplaced`'s own rejection)
/// instead of silently skipping lowering entirely -- left unchecked, a
/// misplaced `break` with nothing else to trigger lowering would reach
/// firrtl.rs as a bare `Stmt::Break` that nothing there recognizes,
/// compiling away to nothing rather than erroring.
fn find_break_anywhere(ast: &Ast, stmts: &[StmtId]) -> Option<Span> {
    for stmt in stmts {
        match ast.stmt(*stmt) {
            Stmt::Break => return Some(ast.stmt_spans[stmt.0 as usize].clone()),
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                if let Some(span) = find_break_anywhere(ast, then_body) {
                    return Some(span);
                }
                if let Some(span) = else_body
                    .as_deref()
                    .and_then(|b| find_break_anywhere(ast, b))
                {
                    return Some(span);
                }
            }
            Stmt::IfLet {
                then_body,
                else_body,
                ..
            } => {
                if let Some(span) = find_break_anywhere(ast, then_body) {
                    return Some(span);
                }
                if let Some(span) = else_body
                    .as_deref()
                    .and_then(|b| find_break_anywhere(ast, b))
                {
                    return Some(span);
                }
            }
            Stmt::While { body, .. } => {
                if let Some(span) = find_break_anywhere(ast, body) {
                    return Some(span);
                }
            }
            Stmt::WhileLet { body, .. } => {
                if let Some(span) = find_break_anywhere(ast, body) {
                    return Some(span);
                }
            }
            _ => {}
        }
    }
    None
}

/// Every `Expr::Guard` node reachable from `root` by descending ONLY
/// through `Expr::Guard`/`Expr::Field` -- the `?.` chain's own spine
/// (`a?.b?.c`, any number of hops, each `?` folding into the rule's
/// guard independently). A `Guard` reached by descending into anything
/// else (an arithmetic operand, a call argument, an `if` condition)
/// isn't part of this spine. Shared by `firrtl/checks.rs` (every OTHER
/// guard, found via the general `sub_exprs` walk below, stays rejected)
/// and `firrtl/writes.rs` (every guard ON this spine gets folded into
/// the rule's own guard, not just the outermost one).
pub(crate) fn guard_chain_spine(ast: &Ast, root: ExprId) -> Vec<ExprId> {
    fn walk(ast: &Ast, id: ExprId, out: &mut Vec<ExprId>) {
        match ast.expr(id) {
            Expr::Guard(inner) => {
                out.push(id);
                walk(ast, *inner, out);
            }
            Expr::Field { base, .. } => walk(ast, *base, out),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(ast, root, &mut out);
    out
}

/// Direct expression children the parser can produce, one level.
pub(crate) fn sub_exprs(ast: &Ast, id: ExprId) -> Vec<ExprId> {
    match ast.expr(id).clone() {
        Expr::Ident(_) | Expr::Int(_) | Expr::SizedInt { .. } | Expr::Wildcard | Expr::Absent => {
            vec![]
        }
        Expr::OptionTy(inner) => vec![inner],
        Expr::Unary { operand, .. } => vec![operand],
        Expr::Binary { lhs, rhs, .. } => vec![lhs, rhs],
        Expr::Guard(inner) | Expr::Spawn(inner) | Expr::Optional(inner) | Expr::Logic(inner) => {
            vec![inner]
        }
        Expr::Field { base, .. } => vec![base],
        Expr::Call { callee, args } | Expr::Bracket { callee, args } => {
            let mut v = vec![callee];
            v.extend(args);
            v
        }
        Expr::ListLit(items) => items,
        Expr::Range { lo, hi } => [lo, hi].into_iter().flatten().collect(),
        Expr::Or(alts) => alts,
        Expr::StructLit { name, fields, base } => {
            let mut v = vec![name];
            v.extend(fields.into_iter().map(|(_, value)| value));
            v.extend(base);
            v
        }
    }
}
