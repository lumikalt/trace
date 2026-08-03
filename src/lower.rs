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

use crate::ast::{Ast, Expr, ExprId, Item, ItemId, Param, Stmt, StmtId};
use crate::effects::Effects;
use crate::lexer::Span;
use crate::resolve::{DefId, DefKind, Resolution};
use crate::types::{Ty, Types, Width};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

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
                if !body.iter().any(|s| matches!(ast.stmt(*s), Stmt::Tick)) {
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

fn plan_rule(
    ast: &Ast,
    res: &Resolution,
    types: &Types,
    def_items: &HashMap<DefId, ItemId>,
    rule: ItemId,
    rule_name: &str,
    body: &[StmtId],
) -> Result<LoweredRule, Vec<LowerError>> {
    // Each of these three checks scans the whole body; run them in order
    // and stop at the first hit rather than accumulating, since a nested
    // spawn (say) would otherwise ALSO get re-flagged by the generic
    // unsupported-construct scan below, as two reports of one problem.
    if let Some(span) = find_nested_tick(ast, body) {
        return Err(vec![LowerError {
            span,
            message: "`tick` must be at the top level of a sequences rule, not nested in \
                      if/while (v0 restriction)"
                .to_string(),
        }]);
    }
    if let Some(span) = find_nested_spawn(ast, body) {
        return Err(vec![LowerError {
            span,
            message: "`spawn` must be at the top level of a sequences rule, not nested in \
                      if/while (v0 restriction, same as `tick`)"
                .to_string(),
        }]);
    }
    if let Some(span) = find_unsupported_construct(ast, res, body, true) {
        return Err(vec![LowerError {
            span,
            message: "unsupported use of `spawn`/`sync`/`race` (v0 restriction): `spawn`'s \
                      result must be bound directly to a fresh local (`h := spawn \
                      Callee(args)`), and `sync[...]`/`race[...]` must be written as their \
                      own statement or as a `tick`'s trailing expression (`tick sync[...]`, \
                      `tick race[...]`)"
                .to_string(),
        }]);
    }

    let mut errors = Vec::new();

    // Split at top-level ticks.
    let segments = split_into_segments(ast, body);

    // Spawn-trigger and sync-call statements, found at the top level of
    // each segment (never nested — `find_nested_spawn` above already
    // confirmed no spawn hides in if/while, and a nested sync would have
    // been caught by `find_unsupported_construct` above since only a
    // bare top-level shape is recognized).
    let mut handle_defs: HashSet<DefId> = HashSet::new();
    let mut spawn_sites: Vec<(StmtId, DefId, String, ExprId)> = Vec::new();
    let mut syncs: Vec<(StmtId, Vec<DefId>)> = Vec::new();
    let mut races: Vec<(StmtId, Vec<DefId>)> = Vec::new();
    for seg in &segments {
        for stmt in &seg.stmts {
            if let Some((handle_def, handle_name, call)) = spawn_trigger_shape(ast, res, *stmt) {
                // `:=` binds a local only when unresolved, so a SECOND
                // `h := spawn ...` reuses the exact same `handle_def` as
                // the first rather than shadowing it — each occurrence
                // needs its own private `__cont_*`/`__done_*`/etc.
                // register set (see `plan_spawn`), so two spawns sharing
                // one handle would plan and render TWO conflicting
                // register/rule definitions under the identical name.
                // Left uncaught, this surfaces several passes later as a
                // confusing "already defined" resolve error on an
                // auto-generated register name the user never wrote,
                // rather than pointing at the actual mistake.
                if !handle_defs.insert(handle_def) {
                    return Err(vec![LowerError {
                        span: ast.stmt_spans[stmt.0 as usize].clone(),
                        message: format!(
                            "`{handle_name}` already names an earlier `spawn` in this rule; \
                             each spawn needs its own handle — give this one a different name"
                        ),
                    }]);
                }
                spawn_sites.push((*stmt, handle_def, handle_name, call));
            } else if let Some(handles) = sync_call_shape(ast, res, *stmt) {
                syncs.push((*stmt, handles));
            } else if let Some(handles) = race_call_shape(ast, res, *stmt) {
                races.push((*stmt, handles));
            } else if let Some(handles) = race_value_shape(ast, res, *stmt) {
                races.push((*stmt, handles));
            }
        }
    }

    let mut spawns = Vec::new();
    for (trigger_stmt, handle_def, handle_name, call) in spawn_sites {
        match plan_spawn(
            ast,
            res,
            types,
            def_items,
            rule_name,
            &handle_name,
            handle_def,
            trigger_stmt,
            call,
        ) {
            Ok(plan) => spawns.push(plan),
            Err(errs) => errors.extend(errs),
        }
    }

    let captures = match compute_captures(ast, res, types, &segments, &handle_defs) {
        Ok(c) => c,
        Err(errs) => {
            errors.extend(errs);
            Vec::new()
        }
    };

    if !errors.is_empty() {
        return Err(errors);
    }

    let handle_names: HashMap<DefId, (String, String)> = spawns
        .iter()
        .map(|s| (s.handle_def, (s.result_name.clone(), s.done_name.clone())))
        .collect();
    let mut handle_field_rewrites = collect_handle_field_rewrites(ast, res, body, &handle_names);
    // A `let`-bound capture's own declaring statement needs rewriting to
    // `{name} := ` before its (otherwise verbatim-spliced) text can be
    // re-resolved as a write to the `reg {name}` this rule's own preamble
    // declares for it (see `CapturedLocal::let_prefix_span`'s own doc
    // comment). Folded into the SAME edits list `splice()` already
    // applies to every ordinary statement below, rather than a second
    // field/call site.
    for cap in &captures {
        if let Some(span) = &cap.let_prefix_span {
            handle_field_rewrites.push((span.clone(), format!("{} := ", cap.name)));
        }
    }

    let cont_width = clog2(segments.len() as u64).max(1);
    Ok(LoweredRule {
        rule,
        rule_name: rule_name.to_string(),
        cont_name: format!("__cont_{rule_name}"),
        cont_width,
        segments,
        captures,
        spawns,
        syncs,
        races,
        handle_field_rewrites,
    })
}

/// Every `Return` statement reachable from `stmt`, including itself and
/// any nested inside if/while — used to catch an early return hiding
/// inside a conditional, which a purely top-level scan would miss.
fn find_returns(ast: &Ast, stmt: StmtId) -> Vec<StmtId> {
    let mut out = Vec::new();
    match ast.stmt(stmt) {
        Stmt::Return(_) => out.push(stmt),
        Stmt::If {
            then_body,
            else_body,
            ..
        } => {
            for s in then_body {
                out.extend(find_returns(ast, *s));
            }
            if let Some(else_body) = else_body {
                for s in else_body {
                    out.extend(find_returns(ast, *s));
                }
            }
        }
        Stmt::While { body, .. } => {
            for s in body {
                out.extend(find_returns(ast, *s));
            }
        }
        _ => {}
    }
    out
}

fn split_into_segments(ast: &Ast, body: &[StmtId]) -> Vec<Segment> {
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
    segments
}

/// Recognizes `let h = spawn Callee(args)` (the ordinary form now that
/// `let` is required for every fresh local) or `h := spawn Callee(args)`
/// (still recognized too — an already-`let`-bound `h` reassigned to a
/// second spawn is nonsensical given the one-spawn-per-handle rule
/// below, but there's no reason to reject the shape itself here).
/// Neither form needs `render_spawn_trigger` to splice this statement's
/// own text at all — it synthesizes brand-new register-write lines from
/// the extracted `(def, name, inner call)` alone, so unlike an ordinary
/// captured local, a `let`-bound handle needs no prefix-span rewrite.
fn spawn_trigger_shape(
    ast: &Ast,
    res: &Resolution,
    stmt: StmtId,
) -> Option<(DefId, String, ExprId)> {
    match ast.stmt(stmt) {
        Stmt::Assign { lhs, rhs } => {
            let Expr::Ident(_) = ast.expr(*lhs) else {
                return None;
            };
            let Expr::Spawn(inner) = ast.expr(*rhs) else {
                return None;
            };
            let def = *res.expr_defs.get(lhs)?;
            if res.def(def).kind != DefKind::Local {
                return None;
            }
            Some((def, res.def(def).name.clone(), *inner))
        }
        Stmt::Let { name, init } => {
            let Expr::Spawn(inner) = ast.expr(*init) else {
                return None;
            };
            let (idx, _) = res
                .defs
                .iter()
                .enumerate()
                .find(|(_, d)| d.span == name.span)?;
            Some((DefId(idx as u32), name.text.clone(), *inner))
        }
        _ => None,
    }
}

/// Recognizes `sync[h1, h2, ...]` written as its own bare statement
/// (including the leading statement of a segment a `tick sync[...]`
/// desugars into — see `parser.rs`'s `tick`-with-expr handling).
fn sync_call_shape(ast: &Ast, res: &Resolution, stmt: StmtId) -> Option<Vec<DefId>> {
    handle_bracket_call_shape(ast, res, stmt, "sync")
}

/// Recognizes `race[h1, h2, ...]` written as its own bare statement, the
/// same shape `sync` uses (a bracket-call naming handles, as its own
/// top-level statement) — `race` differs only in what it lowers to
/// (every named handle's OWN segments gain an extra "no other named
/// competitor has already finished" guard, computed in `plan_rule`; see
/// its own doc comment there for why this needs no separate cancellation
/// register).
fn race_call_shape(ast: &Ast, res: &Resolution, stmt: StmtId) -> Option<Vec<DefId>> {
    handle_bracket_call_shape(ast, res, stmt, "race")
}

/// Recognizes `let value = race[h1, h2, ...]` (or `value := race[...]`)
/// — the value-producing form: `value` gets whichever named handle
/// actually won, instead of `race` only gating the segment. Still gets
/// the SAME cancellation as the guard-only form (`plan_rule` folds this
/// into the same `races` list, since cancellation doesn't care which
/// shape named the handles) — see `render_rule`'s own doc comment on
/// both.
fn race_value_shape(ast: &Ast, res: &Resolution, stmt: StmtId) -> Option<Vec<DefId>> {
    // `let value = race[...]` (now the ordinary form) or `value :=
    // race[...]` (still recognized, same reasoning as `spawn_trigger_
    // shape`'s own two-shape match). `render_rule`'s own race-value
    // render site re-derives the destination name directly from `stmt`,
    // so this fn only needs the racing HANDLES, not the destination.
    match ast.stmt(stmt) {
        Stmt::Assign { lhs, rhs } => {
            let Expr::Ident(_) = ast.expr(*lhs) else {
                return None;
            };
            let Expr::Bracket { callee, args } = ast.expr(*rhs) else {
                return None;
            };
            bracket_call_handles(ast, res, *callee, args, "race")
        }
        Stmt::Let { init, .. } => {
            let Expr::Bracket { callee, args } = ast.expr(*init) else {
                return None;
            };
            bracket_call_handles(ast, res, *callee, args, "race")
        }
        _ => None,
    }
}

fn handle_bracket_call_shape(
    ast: &Ast,
    res: &Resolution,
    stmt: StmtId,
    name: &str,
) -> Option<Vec<DefId>> {
    let Stmt::Expr(e) = ast.stmt(stmt) else {
        return None;
    };
    let Expr::Bracket { callee, args } = ast.expr(*e) else {
        return None;
    };
    bracket_call_handles(ast, res, *callee, args, name)
}

fn bracket_call_handles(
    ast: &Ast,
    res: &Resolution,
    callee: ExprId,
    args: &[ExprId],
    name: &str,
) -> Option<Vec<DefId>> {
    let def = res.expr_defs.get(&callee)?;
    let d = res.def(*def);
    if d.kind != DefKind::Builtin || d.name != name {
        return None;
    }
    let mut handles = Vec::new();
    for arg in args {
        let Expr::Ident(_) = ast.expr(*arg) else {
            return None;
        };
        handles.push(*res.expr_defs.get(arg)?);
    }
    Some(handles)
}

/// Plan one `spawn` occurrence: cut the callee's own `<sequences>` body
/// into segments (same algorithm as a rule), rename its params and
/// captured locals into this occurrence's private registers.
#[allow(clippy::too_many_arguments)]
fn plan_spawn(
    ast: &Ast,
    res: &Resolution,
    types: &Types,
    def_items: &HashMap<DefId, ItemId>,
    rule_name: &str,
    handle_name: &str,
    handle_def: DefId,
    trigger_stmt: StmtId,
    call: ExprId,
) -> Result<SpawnPlan, Vec<LowerError>> {
    let span = ast.expr_spans[call.0 as usize].clone();
    let Expr::Call {
        callee,
        args: call_args,
    } = ast.expr(call).clone()
    else {
        return Err(vec![LowerError {
            span,
            message: "`spawn` needs a direct function call, e.g. `spawn Foo(a, b)`".to_string(),
        }]);
    };
    let Some(callee_def) = res.expr_defs.get(&callee).copied() else {
        return Err(vec![LowerError {
            span,
            message: "spawn callee did not resolve".to_string(),
        }]);
    };
    let Some(&callee_item) = def_items.get(&callee_def) else {
        return Err(vec![LowerError {
            span,
            message: "spawn callee is not a function".to_string(),
        }]);
    };
    let Item::Fn {
        params,
        body: callee_body,
        ..
    } = ast.item(callee_item).clone()
    else {
        return Err(vec![LowerError {
            span,
            message: "spawn callee is not a function".to_string(),
        }]);
    };

    if let Some(s) = find_nested_tick(ast, &callee_body) {
        return Err(vec![LowerError {
            span: s,
            message: "`tick` must be at the top level of a spawned fn's body, not nested in \
                      if/while (v0 restriction)"
                .to_string(),
        }]);
    }
    if let Some(s) = find_nested_spawn(ast, &callee_body) {
        return Err(vec![LowerError {
            span: s,
            message: "a spawned fn cannot itself `spawn` (v0 restriction: no nested \
                      parallelism)"
                .to_string(),
        }]);
    }
    if let Some(s) = find_unsupported_construct(ast, res, &callee_body, true) {
        return Err(vec![LowerError {
            span: s,
            message: "sequences lowering does not yet support `race`, or a nested \
                      `spawn`/`sync`, inside a spawned fn's body (v0 restriction)"
                .to_string(),
        }]);
    }

    let mut errors = Vec::new();
    let segments = split_into_segments(ast, &callee_body);
    let nsegs = segments.len() as u64;

    // `return` must be the last statement of the last segment; no early
    // return anywhere else, including nested inside if/while.
    let mut return_expr = None;
    for (i, seg) in segments.iter().enumerate() {
        for (j, stmt) in seg.stmts.iter().enumerate() {
            let is_last_stmt_of_last_segment = i + 1 == segments.len() && j + 1 == seg.stmts.len();
            for nested in find_returns(ast, *stmt) {
                let is_last = is_last_stmt_of_last_segment && nested == *stmt;
                if !is_last {
                    errors.push(LowerError {
                        span: ast.stmt_spans[nested.0 as usize].clone(),
                        message: "a spawned fn's `return` must be the last statement of its \
                                  last segment (v0 restriction: no early return)"
                            .to_string(),
                    });
                    continue;
                }
                let Stmt::Return(value) = ast.stmt(nested) else {
                    unreachable!()
                };
                match value {
                    Some(v) => return_expr = Some(*v),
                    None => errors.push(LowerError {
                        span: ast.stmt_spans[nested.0 as usize].clone(),
                        message: "a spawned fn must `return` a value; `.result` needs \
                                  something to read"
                            .to_string(),
                    }),
                }
            }
        }
    }
    let Some(return_expr) = return_expr else {
        if errors.is_empty() {
            errors.push(LowerError {
                span,
                message: "a spawned fn's last segment must end with `return <value>`".to_string(),
            });
        }
        return Err(errors);
    };
    // The `return` statement itself isn't ordinary rule-body syntax;
    // `render_spawn_segments` recognizes it by position (last statement
    // of the last segment, already validated above) and emits a
    // `result := ...` write in its place instead of splicing it verbatim.

    let param_defs: Vec<(DefId, &Param)> = params
        .iter()
        .filter_map(|p| {
            res.defs
                .iter()
                .enumerate()
                .find(|(_, d)| d.span == p.name.span)
                .map(|(i, _)| (DefId(i as u32), p))
        })
        .collect();
    let param_def_set: HashSet<DefId> = param_defs.iter().map(|(d, _)| *d).collect();

    if call_args.len() != param_defs.len() {
        errors.push(LowerError {
            span,
            message: format!(
                "spawn callee expects {} argument(s), got {}",
                param_defs.len(),
                call_args.len()
            ),
        });
        return Err(errors);
    }

    let mut renames: HashMap<DefId, String> = HashMap::new();
    let mut args = Vec::new();
    for ((def, param), arg_expr) in param_defs.iter().zip(call_args.iter()) {
        let reg_name = format!("__arg_{rule_name}_{handle_name}_{}", param.name.text);
        let ty = types.local_tys.get(def).cloned().unwrap_or(Ty::Unknown);
        renames.insert(*def, reg_name.clone());
        args.push(SpawnArg {
            reg_name,
            ty,
            caller_expr: *arg_expr,
        });
    }

    let captures = match compute_captures(ast, res, types, &segments, &param_def_set) {
        Ok(c) => c,
        Err(errs) => {
            errors.extend(errs);
            Vec::new()
        }
    };
    if !errors.is_empty() {
        return Err(errors);
    }
    for cap in &captures {
        renames.insert(
            cap.def,
            format!("__save_{rule_name}_{handle_name}_{}", cap.name),
        );
    }

    let result_ty = types
        .expr_tys
        .get(&return_expr)
        .cloned()
        .unwrap_or(Ty::Unknown);
    if !matches!(result_ty, Ty::Bits(Width::Known(_))) {
        errors.push(LowerError {
            span: ast.expr_spans[return_expr.0 as usize].clone(),
            message: format!(
                "spawn's return value has type {result_ty}, not a concrete `[w]`; \
                 sequences lowering needs a known width to declare `.result`'s register"
            ),
        });
        return Err(errors);
    }

    let mut rename_edits = collect_renames(ast, res, &callee_body, &renames);
    // `collect_renames`/`collect_renames_expr` only ever rewrite
    // EXPRESSION positions (`stmt_exprs`), so a capture's OWN declaring
    // `let name = init` never gets its `name` renamed to `__save_...` —
    // `Stmt::Let`'s `name` field is a bare `Name`, not an `ExprId`, so
    // there's nothing there for that walk to match against `renames` at
    // all. Same fix as `render_rule`'s top-level captures (`let_prefix_
    // span`), just targeting the RENAMED save-register name instead of
    // the plain original one, since a spawn callee's captures always go
    // through the rename scheme (never reuse their own source name).
    for cap in &captures {
        if let Some(span) = &cap.let_prefix_span {
            let target = renames.get(&cap.def).cloned().unwrap_or(cap.name.clone());
            rename_edits.push((span.clone(), format!("{target} := ")));
        }
    }

    // No sentinel/idle reset value: unlike a first glance suggests, a
    // plain 0 reset (matching every other continuation register in this
    // emitter) is sound here too, checked by hand-simulating a gated-
    // trigger variant before settling on this. A spurious pre-trigger
    // completion (segment-0 guard reading true before ever being
    // spawned) is structurally unobservable — `render_rule` always
    // emits the trigger's own segment rules before a spawn's callee
    // segments, so derived-stall priority always favors the trigger on
    // any shared cycle; the trigger itself resets `done` to 0 whenever
    // it fires, overwriting any spurious completion; and `sync` can
    // never observe `done` before the trigger's own segment has fired
    // at least once, since it's gated behind the rule's own mandatory
    // `tick`.
    let cont_width = clog2(nsegs).max(1);
    Ok(SpawnPlan {
        handle_def,
        handle_name: handle_name.to_string(),
        trigger_stmt,
        cont_name: format!("__cont_{rule_name}_{handle_name}"),
        cont_width,
        done_name: format!("__done_{rule_name}_{handle_name}"),
        result_name: format!("__result_{rule_name}_{handle_name}"),
        result_ty,
        args,
        segments,
        captures,
        renames,
        rename_edits,
        return_expr,
        base_rule_name: rule_name.to_string(),
    })
}

/// Shared by a rule's own body and a spawn callee's body: per-local
/// def/use segments, single-assignment + read-only-later validation,
/// concrete-width requirement. `exclude` skips defs handled by a
/// different mechanism (spawn handles at rule level, params at callee
/// level).
fn compute_captures(
    ast: &Ast,
    res: &Resolution,
    types: &Types,
    segments: &[Segment],
    exclude: &HashSet<DefId>,
) -> Result<Vec<CapturedLocal>, Vec<LowerError>> {
    let mut assigns: BTreeMap<DefId, BTreeSet<usize>> = Default::default();
    let mut reads: BTreeMap<DefId, BTreeSet<usize>> = Default::default();
    let mut let_bound: HashMap<DefId, Span> = Default::default();
    for seg in segments {
        scan_stmts(
            ast,
            res,
            &seg.stmts,
            seg.index as usize,
            &mut assigns,
            &mut reads,
            &mut let_bound,
        );
    }
    assigns.retain(|d, _| !exclude.contains(d));
    reads.retain(|d, _| !exclude.contains(d));

    let mut errors = Vec::new();
    let mut captures = Vec::new();
    let mut capture_names: HashMap<String, DefId> = Default::default();
    for (def, assign_segs) in &assigns {
        let read_segs = reads.get(def).cloned().unwrap_or_default();
        let touched: BTreeSet<usize> = assign_segs.union(&read_segs).copied().collect();
        if touched.len() <= 1 {
            continue; // stays a plain local within one generated segment
        }
        let name = res.def(*def).name.clone();
        let let_prefix_span = let_bound.get(def).cloned();
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
                    "`{name}` has type {ty}, not a concrete `[w]`; sequences lowering \
                     needs a known width to declare its save register"
                ),
            });
            continue;
        }
        if let Some(_prev) = capture_names.get(&name) {
            errors.push(LowerError {
                span: res.def(*def).span.clone(),
                message: format!(
                    "`{name}` shadows another captured value of the same name across this \
                     rule's segments; sequences lowering needs a distinct name per captured \
                     save register (v0 restriction: rename one of the `let {name}`s)"
                ),
            });
            continue;
        }
        capture_names.insert(name.clone(), *def);
        captures.push(CapturedLocal {
            def: *def,
            name,
            ty,
            assign_segment,
            read_segments: read_segs.into_iter().collect(),
            let_prefix_span,
        });
    }

    if errors.is_empty() {
        Ok(captures)
    } else {
        Err(errors)
    }
}

/// Every `h.result`/`h.done` reference anywhere in `stmts` (recursing
/// through if/else), rewritten to the owning spawn's register name.
fn collect_handle_field_rewrites(
    ast: &Ast,
    res: &Resolution,
    stmts: &[StmtId],
    handles: &HashMap<DefId, (String, String)>,
) -> Vec<(Span, String)> {
    let mut edits = Vec::new();
    for stmt in stmts {
        for e in stmt_exprs(ast, *stmt) {
            collect_handle_fields_expr(ast, res, e, handles, &mut edits);
        }
        match ast.stmt(*stmt) {
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                edits.extend(collect_handle_field_rewrites(ast, res, then_body, handles));
                if let Some(else_body) = else_body {
                    edits.extend(collect_handle_field_rewrites(ast, res, else_body, handles));
                }
            }
            Stmt::While { body, .. } => {
                edits.extend(collect_handle_field_rewrites(ast, res, body, handles));
            }
            _ => {}
        }
    }
    edits
}

fn collect_handle_fields_expr(
    ast: &Ast,
    res: &Resolution,
    id: ExprId,
    handles: &HashMap<DefId, (String, String)>,
    edits: &mut Vec<(Span, String)>,
) {
    if let Expr::Field { base, name } = ast.expr(id).clone()
        && let Some(def) = res.expr_defs.get(&base)
        && let Some((result_name, done_name)) = handles.get(def)
    {
        let replacement = match name.as_str() {
            "result" => result_name.clone(),
            "done" => done_name.clone(),
            _ => return,
        };
        edits.push((ast.expr_spans[id.0 as usize].clone(), replacement));
        return;
    }
    for child in sub_exprs(ast, id) {
        collect_handle_fields_expr(ast, res, child, handles, edits);
    }
}

/// Called on a body's top level: top-level ticks are fine, only descends
/// into if/while to reject ticks hiding inside them.
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

/// Same shape as `find_nested_tick`, for `spawn`: legal at a body's top
/// level, never inside if/while.
fn find_nested_spawn(ast: &Ast, stmts: &[StmtId]) -> Option<Span> {
    for stmt in stmts {
        match ast.stmt(*stmt) {
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                if let Some(span) = find_spawn_anywhere(ast, then_body) {
                    return Some(span);
                }
                if let Some(span) = else_body
                    .as_deref()
                    .and_then(|b| find_spawn_anywhere(ast, b))
                {
                    return Some(span);
                }
            }
            Stmt::While { body, .. } => {
                if let Some(span) = find_spawn_anywhere(ast, body) {
                    return Some(span);
                }
            }
            _ => {}
        }
    }
    None
}

fn find_spawn_anywhere(ast: &Ast, stmts: &[StmtId]) -> Option<Span> {
    for stmt in stmts {
        for e in stmt_exprs(ast, *stmt) {
            if let Some(span) = find_spawn_in_expr(ast, e) {
                return Some(span);
            }
        }
        match ast.stmt(*stmt) {
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                if let Some(span) = find_spawn_anywhere(ast, then_body) {
                    return Some(span);
                }
                if let Some(span) = else_body
                    .as_deref()
                    .and_then(|b| find_spawn_anywhere(ast, b))
                {
                    return Some(span);
                }
            }
            Stmt::While { body, .. } => {
                if let Some(span) = find_spawn_anywhere(ast, body) {
                    return Some(span);
                }
            }
            _ => {}
        }
    }
    None
}

fn find_spawn_in_expr(ast: &Ast, id: ExprId) -> Option<Span> {
    if let Expr::Spawn(_) = ast.expr(id) {
        return Some(ast.expr_spans[id.0 as usize].clone());
    }
    for child in sub_exprs(ast, id) {
        if let Some(span) = find_spawn_in_expr(ast, child) {
            return Some(span);
        }
    }
    None
}

/// Scans for anything this pass still can't handle: `spawn`/`sync`/
/// `race` used in any shape other than the four legitimate ones
/// (`let h = spawn Callee(args)` or `h := spawn Callee(args)`,
/// `sync[...]`/`race[...]` as their own statement — which also covers a
/// `tick sync[...]`/`tick race[...]`'s desugared second statement — and
/// `let value = race[...]`/`value := race[...]`) — those four are
/// recognized and skipped by the caller before reaching here.
fn find_unsupported_construct(
    ast: &Ast,
    res: &Resolution,
    stmts: &[StmtId],
    top_level: bool,
) -> Option<Span> {
    for stmt in stmts {
        let is_spawn_trigger = top_level && spawn_trigger_shape(ast, res, *stmt).is_some();
        let is_sync_call = top_level && sync_call_shape(ast, res, *stmt).is_some();
        let is_race_call = top_level && race_call_shape(ast, res, *stmt).is_some();
        let is_race_value = top_level && race_value_shape(ast, res, *stmt).is_some();

        // `let x = expr` or `x := expr` both bind via their own second
        // expr slot (`stmt_exprs` returns `[init]` for `Let`, `[lhs,
        // rhs]` for `Assign`) — `.last()` picks the value side for
        // either shape uniformly, since a spawn trigger/race value is
        // never the `Assign` LHS case (`spawn_trigger_shape`/`race_
        // value_shape` already confirmed this statement IS one of
        // those, so the value expr is always present here).
        if is_spawn_trigger {
            let value = *stmt_exprs(ast, *stmt).last().unwrap();
            let Expr::Spawn(inner) = ast.expr(value) else {
                unreachable!()
            };
            for child in sub_exprs(ast, *inner) {
                if let Some(span) = find_unsupported_in_expr(ast, res, child) {
                    return Some(span);
                }
            }
        } else if is_sync_call || is_race_call {
            let Stmt::Expr(e) = ast.stmt(*stmt) else {
                unreachable!()
            };
            let Expr::Bracket { args, .. } = ast.expr(*e) else {
                unreachable!()
            };
            for arg in args {
                if let Some(span) = find_unsupported_in_expr(ast, res, *arg) {
                    return Some(span);
                }
            }
        } else if is_race_value {
            let value = *stmt_exprs(ast, *stmt).last().unwrap();
            let Expr::Bracket { args, .. } = ast.expr(value) else {
                unreachable!()
            };
            for arg in args {
                if let Some(span) = find_unsupported_in_expr(ast, res, *arg) {
                    return Some(span);
                }
            }
        } else {
            for e in stmt_exprs(ast, *stmt) {
                if let Some(span) = find_unsupported_in_expr(ast, res, e) {
                    return Some(span);
                }
            }
        }

        let nested = match ast.stmt(*stmt) {
            Stmt::If {
                then_body,
                else_body,
                ..
            } => find_unsupported_construct(ast, res, then_body, false).or_else(|| {
                else_body
                    .as_deref()
                    .and_then(|b| find_unsupported_construct(ast, res, b, false))
            }),
            Stmt::While { body, .. } => find_unsupported_construct(ast, res, body, false),
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
        Expr::Call { callee, .. } | Expr::Bracket { callee, .. } => {
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
        Stmt::IfLet { init, .. } => vec![init],
        Stmt::While { cond, .. } => vec![cond],
    }
}

fn scan_stmts(
    ast: &Ast,
    res: &Resolution,
    stmts: &[StmtId],
    segment: usize,
    assigns: &mut BTreeMap<DefId, BTreeSet<usize>>,
    reads: &mut BTreeMap<DefId, BTreeSet<usize>>,
    let_bound: &mut HashMap<DefId, Span>,
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
                    let def = DefId(idx as u32);
                    assigns.entry(def).or_default().insert(segment);
                    // A `let`-bound value CAN cross a tick, but its
                    // declaring statement needs a render-time REWRITE
                    // first: splice-based lowering makes a captured local
                    // survive by declaring a `reg` of the same name (or,
                    // for a spawn-callee capture, a renamed `__save_...`
                    // one) and then splicing the ORIGINAL binding
                    // statement's text verbatim — true by accident for
                    // `x := value` (still parses as a register write once
                    // that `reg` exists), never true for `let x = value`
                    // (always binds a FRESH local, shadowing the register
                    // rather than writing it, per resolve.rs's own
                    // define-vs-mutate split). The PREFIX span recorded
                    // here — from this statement's own start through to
                    // `init`'s own start, i.e. exactly `let name = ` — is
                    // what `render_rule`/`plan_spawn` replace with
                    // `{target_name} := ` before splicing, turning `let x
                    // = expr` into `x := expr` (or the renamed
                    // equivalent) at render time without ever touching
                    // the ORIGINAL source the user wrote.
                    let stmt_span = ast.stmt_spans[stmt.0 as usize].clone();
                    let init_span = ast.expr_spans[init.0 as usize].clone();
                    let_bound.insert(def, stmt_span.start..init_span.start);
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
                scan_stmts(ast, res, &then_body, segment, assigns, reads, let_bound);
                if let Some(else_body) = else_body {
                    scan_stmts(ast, res, &else_body, segment, assigns, reads, let_bound);
                }
            }
            // `name` is deliberately NOT registered in `assigns`/
            // `let_bound` the way `Stmt::Let`'s own arm above does: that
            // machinery exists to let a render-time REWRITE turn `let
            // name = init` into `name := init` verbatim when a local
            // needs to survive a `tick` as a synthesized register --
            // `if let name = init { ... }` has no such rewrite (the
            // surrounding `if`/branch structure can't just disappear the
            // way a bare `let` statement can), so `name` crossing a
            // `tick` inside its own `then_body` isn't supported by this
            // pass. `init` itself is scanned for reads only, same
            // treatment `Stmt::If`'s `cond` already gets.
            Stmt::IfLet {
                init,
                then_body,
                else_body,
                ..
            } => {
                scan_expr(ast, res, init, segment, reads);
                scan_stmts(ast, res, &then_body, segment, assigns, reads, let_bound);
                if let Some(else_body) = else_body {
                    scan_stmts(ast, res, &else_body, segment, assigns, reads, let_bound);
                }
            }
            Stmt::While { cond, body } => {
                scan_expr(ast, res, cond, segment, reads);
                scan_stmts(ast, res, &body, segment, assigns, reads, let_bound);
            }
        }
    }
}

fn scan_expr(
    ast: &Ast,
    res: &Resolution,
    id: ExprId,
    segment: usize,
    reads: &mut BTreeMap<DefId, BTreeSet<usize>>,
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

/// Collects `(span, replacement)` edits for every `Expr::Ident` in
/// `stmts` whose resolved def is a key in `renames`.
fn collect_renames(
    ast: &Ast,
    res: &Resolution,
    stmts: &[StmtId],
    renames: &HashMap<DefId, String>,
) -> Vec<(Span, String)> {
    let mut edits = Vec::new();
    for stmt in stmts {
        for e in stmt_exprs(ast, *stmt) {
            collect_renames_expr(ast, res, e, renames, &mut edits);
        }
        match ast.stmt(*stmt) {
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                edits.extend(collect_renames(ast, res, then_body, renames));
                if let Some(else_body) = else_body {
                    edits.extend(collect_renames(ast, res, else_body, renames));
                }
            }
            Stmt::While { body, .. } => {
                edits.extend(collect_renames(ast, res, body, renames));
            }
            _ => {}
        }
    }
    edits
}

fn collect_renames_expr(
    ast: &Ast,
    res: &Resolution,
    id: ExprId,
    renames: &HashMap<DefId, String>,
    edits: &mut Vec<(Span, String)>,
) {
    if let Expr::Ident(_) = ast.expr(id)
        && let Some(def) = res.expr_defs.get(&id)
        && let Some(new_name) = renames.get(def)
    {
        edits.push((ast.expr_spans[id.0 as usize].clone(), new_name.clone()));
        return;
    }
    for child in sub_exprs(ast, id) {
        collect_renames_expr(ast, res, child, renames, edits);
    }
}

/// Applies every edit inside `base` (sorted, non-overlapping by
/// construction — each comes from a distinct `ExprId`'s own span) to
/// `src[base]`, verbatim outside those spans.
fn splice(src: &str, base: &Span, edits: &[(Span, String)]) -> String {
    let mut relevant: Vec<&(Span, String)> = edits
        .iter()
        .filter(|(s, _)| s.start >= base.start && s.end <= base.end)
        .collect();
    relevant.sort_by_key(|(s, _)| s.start);
    let mut out = String::new();
    let mut pos = base.start;
    for (span, text) in relevant {
        out.push_str(&src[pos..span.start]);
        out.push_str(text);
        pos = span.end;
    }
    out.push_str(&src[pos..base.end]);
    out
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

/// `race[h1, h2, ...]`'s own guard (does the enclosing segment advance)
/// is one `((d1 | d2 | ...) = 1)?` line, an OR of every named handle's
/// `done` — the mirror of `sync`'s AND. The actual "loser cancellation"
/// DESIGN.md flagged as undesigned needs no separate latch/register at
/// all: every handle race names gets an EXTRA guard clause on every one
/// of its OWN segments (computed below, threaded into
/// `render_spawn_segments`), requiring that none of its named
/// competitors has ALREADY finished. Reading a competitor's `done`
/// register directly (not a value some other statement wrote and that
/// only becomes visible next cycle) is what makes this exact —
/// `done` IS the resolving signal, so there is no one-cycle lag for a
/// stray write to sneak through. A handle that's genuinely behind is
/// permanently blocked the moment a competitor's `done` becomes
/// visible, never firing another segment (until the enclosing rule
/// re-triggers it, which also resets its competitors' `done` back to
/// 0). Two handles finishing on the exact same cycle both attempt their
/// final segment that cycle; each also reads the other's `done` (still
/// 0 mid-cycle), so both fire — but this makes their final segments
/// mutually conflict (each reads what the other writes), so the
/// ordinary derived-stall scheduler picks exactly one by priority
/// (declaration order, or an explicit `urgency` directive) if they'd
/// otherwise land the same cycle; the loser of THAT tie simply stalls
/// a cycle, then sees the winner's `done` and is permanently blocked —
/// same outcome as any other race, no double-completion. Verified via
/// `--explain-schedule` on a hand-lowered two-spawn example before this
/// was implemented, not assumed.
fn render_rule(ast: &Ast, src: &str, lr: &LoweredRule) -> String {
    let mut out = String::new();
    for cap in &lr.captures {
        out.push_str(&format!("reg {} : {} = 0\n", cap.name, cap.ty));
    }
    for spawn in &lr.spawns {
        render_spawn_regs(&mut out, spawn);
    }
    out.push_str(&format!(
        "reg {} : [{}] = 0\n\n",
        lr.cont_name, lr.cont_width
    ));

    let spawn_by_trigger: HashMap<StmtId, &SpawnPlan> =
        lr.spawns.iter().map(|s| (s.trigger_stmt, s)).collect();
    let sync_by_stmt: HashMap<StmtId, &Vec<DefId>> =
        lr.syncs.iter().map(|(s, h)| (*s, h)).collect();
    let race_by_stmt: HashMap<StmtId, &Vec<DefId>> =
        lr.races.iter().map(|(s, h)| (*s, h)).collect();
    let spawn_by_handle: HashMap<DefId, &SpawnPlan> =
        lr.spawns.iter().map(|s| (s.handle_def, s)).collect();

    // Every handle's competitors (every OTHER handle it's ever named
    // alongside in a `race[...]`, unioned across every such statement),
    // as done-register-name guard clauses to insert into that handle's
    // OWN segments.
    let mut competitor_guards: HashMap<DefId, Vec<String>> = HashMap::new();
    for (_, handles) in &lr.races {
        for &h in handles {
            for &other in handles {
                if other == h {
                    continue;
                }
                if let Some(plan) = spawn_by_handle.get(&other) {
                    competitor_guards
                        .entry(h)
                        .or_default()
                        .push(format!("({} = 0)?", plan.done_name));
                }
            }
        }
    }

    let nsegs = lr.segments.len() as u64;
    for seg in &lr.segments {
        out.push_str(&format!("rule {}_s{} {{\n", lr.rule_name, seg.index));
        out.push_str(&format!("    ({} = {})?\n", lr.cont_name, seg.index));
        for stmt in &seg.stmts {
            if let Some(spawn) = spawn_by_trigger.get(stmt) {
                render_spawn_trigger(&mut out, src, ast, spawn, &lr.handle_field_rewrites);
            } else if let Some(handles) = sync_by_stmt.get(stmt) {
                for h in handles.iter() {
                    if let Some(plan) = spawn_by_handle.get(h) {
                        out.push_str(&format!("    ({} = 1)?\n", plan.done_name));
                    }
                }
            } else if let Some(handles) = race_by_stmt.get(stmt) {
                let dones: Vec<&str> = handles
                    .iter()
                    .filter_map(|h| spawn_by_handle.get(h))
                    .map(|plan| plan.done_name.as_str())
                    .collect();
                if !dones.is_empty() {
                    out.push_str(&format!("    (({}) = 1)?\n", dones.join(" | ")));
                }
                // Value-producing form (`let value = race[...]`, or the
                // still-recognized `value := race[...]`) additionally
                // assigns the winner's own result — `__race_value` is the
                // internal builtin firrtl/expr.rs compiles straight to a
                // priority mux (see its own doc comment for why trace-
                // source `if`/`else` can't do this instead). Guard-only
                // form (`race[...]` as its own statement) stops above.
                // Synthesizes a brand-new declaration line rather than
                // splicing the original statement, so — like a spawn
                // trigger — needs no prefix-span rewrite; but UNLIKE a
                // spawn trigger, the synthesized keyword itself must
                // match the ORIGINAL shape: a `Stmt::Let` destination is
                // a fresh local needing `let name = `, an `Assign` one is
                // either an existing reg/output (needs `:=`) or an
                // already-`let`-bound local being reassigned (also `:=`)
                // — self-caught by trying `let winner = tick race[...]`
                // followed by a same-segment use of `winner`: hardcoding
                // `name := ...` here left `winner` undeclared in the
                // rendered output (a real, caught-before-shipping bug,
                // not a hypothetical).
                let dest = match ast.stmt(*stmt) {
                    Stmt::Assign { lhs, .. } => match ast.expr(*lhs) {
                        Expr::Ident(name) => Some((name.clone(), false)),
                        _ => None,
                    },
                    Stmt::Let { name, .. } => Some((name.text.clone(), true)),
                    _ => None,
                };
                if let Some((name, is_let)) = dest {
                    let flat: Vec<&str> = handles
                        .iter()
                        .filter_map(|h| spawn_by_handle.get(h))
                        .flat_map(|plan| [plan.done_name.as_str(), plan.result_name.as_str()])
                        .collect();
                    let keyword = if is_let { "let " } else { "" };
                    let op = if is_let { "=" } else { ":=" };
                    out.push_str(&format!(
                        "    {keyword}{name} {op} __race_value({})\n",
                        flat.join(", ")
                    ));
                }
            } else {
                let span = ast.stmt_spans[stmt.0 as usize].clone();
                out.push_str(&splice(src, &span, &lr.handle_field_rewrites));
            }
        }
        let next = if seg.index + 1 < nsegs {
            seg.index + 1
        } else {
            0
        };
        out.push_str(&format!("    {} := {next}\n", lr.cont_name));
        out.push_str("}\n\n");
    }

    for spawn in &lr.spawns {
        let extra_guards = competitor_guards
            .get(&spawn.handle_def)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        render_spawn_segments(&mut out, src, ast, spawn, extra_guards);
    }
    out
}

fn render_spawn_regs(out: &mut String, spawn: &SpawnPlan) {
    for arg in &spawn.args {
        out.push_str(&format!("reg {} : {} = 0\n", arg.reg_name, arg.ty));
    }
    for cap in &spawn.captures {
        let name = spawn
            .renames
            .get(&cap.def)
            .cloned()
            .unwrap_or_else(|| cap.name.clone());
        out.push_str(&format!("reg {name} : {} = 0\n", cap.ty));
    }
    out.push_str(&format!("reg {} : [1] = 0\n", spawn.done_name));
    out.push_str(&format!(
        "reg {} : {} = 0\n",
        spawn.result_name, spawn.result_ty
    ));
    out.push_str(&format!(
        "reg {} : [{}] = 0\n",
        spawn.cont_name, spawn.cont_width
    ));
}

fn render_spawn_trigger(
    out: &mut String,
    src: &str,
    ast: &Ast,
    spawn: &SpawnPlan,
    outer_rewrites: &[(Span, String)],
) {
    for arg in &spawn.args {
        let span = ast.expr_spans[arg.caller_expr.0 as usize].clone();
        out.push_str(&format!(
            "    {} := {}\n",
            arg.reg_name,
            splice(src, &span, outer_rewrites)
        ));
    }
    out.push_str(&format!("    {} := 0\n", spawn.done_name));
    out.push_str(&format!("    {} := 0\n", spawn.cont_name));
}

/// `extra_guards` is empty unless `spawn`'s handle is named in some
/// `race[...]` in the enclosing rule — one `(competitor_done = 0)?`
/// per named competitor, added to EVERY one of this spawn's own
/// segments, so a handle that loses a race can never fire another
/// segment again (see `render_rule`'s own doc comment for why this
/// alone is a correct, exact cancellation with no separate latch).
fn render_spawn_segments(
    out: &mut String,
    src: &str,
    ast: &Ast,
    spawn: &SpawnPlan,
    extra_guards: &[String],
) {
    let nsegs = spawn.segments.len() as u64;
    for seg in &spawn.segments {
        out.push_str(&format!(
            "rule {}_{}_s{} {{\n",
            spawn.base_rule_name, spawn.handle_name, seg.index
        ));
        out.push_str(&format!("    ({} = {})?\n", spawn.cont_name, seg.index));
        for guard in extra_guards {
            out.push_str(&format!("    {guard}\n"));
        }
        let is_last = seg.index + 1 == nsegs;
        for stmt in &seg.stmts {
            if is_last && matches!(ast.stmt(*stmt), Stmt::Return(_)) {
                let rspan = ast.expr_spans[spawn.return_expr.0 as usize].clone();
                let rendered = splice(src, &rspan, &spawn.rename_edits);
                out.push_str(&format!("    {} := {}\n", spawn.result_name, rendered));
            } else {
                let span = ast.stmt_spans[stmt.0 as usize].clone();
                out.push_str(&splice(src, &span, &spawn.rename_edits));
            }
        }
        let next = if is_last { 0 } else { seg.index + 1 };
        out.push_str(&format!("    {} := {next}\n", spawn.cont_name));
        if is_last {
            out.push_str(&format!("    {} := 1\n", spawn.done_name));
        }
        out.push_str("}\n\n");
    }
}
