//! Phase 1 ("plan"): analyzes a `<sequences>` rule's (or a `spawn`
//! callee's) AST and builds the `LoweredRule`/`SpawnPlan` this pass's
//! `render` phase (render.rs) later turns into text — every v0-scope
//! validity check (a nested `tick`/`spawn`/`while`, a misplaced `break`,
//! an unsupported `spawn`/`sync`/`race` shape, ...), segment-cutting
//! (`split_into_segments`), capture analysis (`compute_captures`), and
//! `spawn` planning (`plan_spawn`) all live here. See mod.rs's module
//! doc comment for the whole pass's design story.

use super::{CapturedLocal, LowerError, LoweredRule, Segment, SpawnArg, SpawnPlan};
use super::{clog2, find_tick_anywhere, find_while_anywhere, sub_exprs};
use crate::ast::{Ast, Expr, ExprId, Item, ItemId, Param, Stmt, StmtId};
use crate::lexer::Span;
use crate::resolve::{DefId, DefKind, Resolution};
use crate::types::{Ty, Types, Width};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

pub(crate) fn plan_rule(
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
    if let Some(span) = find_nested_while(ast, body) {
        return Err(vec![LowerError {
            span,
            message: "`while` must be at the top level of a sequences rule, not nested in \
                      if/while (v0 restriction, same as `tick`/`spawn`)"
                .to_string(),
        }]);
    }
    if let Some((span, message)) = find_break_misplaced(ast, body, false) {
        return Err(vec![LowerError {
            span,
            message: message.to_string(),
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
        // Pre-existing gap, self-caught while extending this exact
        // function family for `while`: every other recursive scan below
        // (`find_nested_tick`, `find_tick_anywhere`, `find_nested_spawn`,
        // `find_spawn_anywhere`, `find_unsupported_construct`) had the
        // same missing arm, silently exempting an `if let`'s branches
        // from a check meant to apply uniformly to every branch shape —
        // confirmed live by direct probe: `tick` nested inside `if let`
        // surfaced as "a spawned fn's last segment must end with
        // `return`" instead of the clear "must be at the top level, not
        // nested in if/while" message `if`/`while` already get.
        Stmt::IfLet {
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
        Stmt::WhileLet { body, .. } => {
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
        is_while_loop: false,
    }];
    for stmt in body {
        match ast.stmt(*stmt) {
            Stmt::Tick => {
                let next = segments.len() as u64;
                segments.push(Segment {
                    index: next,
                    stmts: Vec::new(),
                    is_while_loop: false,
                });
            }
            // A top-level `while`/`while let` cuts the SAME way `tick`
            // does, but unlike `tick` (a pure separator with no content
            // of its own) it gets a dedicated segment holding exactly
            // itself -- `render_rule`/`render_spawn_segments` unwrap
            // `cond`/`body` (or `name`/`init`/`body`) from this one
            // statement at render time, wrapping `body` in `if COND {
            // ...; cont := SELF } else { cont := NEXT }` (or `if let
            // NAME = EXPR { ... }`) instead of the ordinary
            // straight-line-then-advance shape. `find_nested_while`
            // (called before this fn ever runs) already confirmed
            // neither hides deeper than the top level, so every `Stmt::
            // While`/`Stmt::WhileLet` reaching this loop is one of these
            // dedicated segments, never folded into a straight-line one.
            Stmt::While { .. } | Stmt::WhileLet { .. } => {
                let while_idx = segments.len() as u64;
                segments.push(Segment {
                    index: while_idx,
                    stmts: vec![*stmt],
                    is_while_loop: true,
                });
                let next_idx = segments.len() as u64;
                segments.push(Segment {
                    index: next_idx,
                    stmts: Vec::new(),
                    is_while_loop: false,
                });
            }
            _ => {
                segments.last_mut().unwrap().stmts.push(*stmt);
            }
        }
    }
    segments
}

/// A `<sequences>` rule/fn's own checked cycle count, when it has one --
/// `None` for a body whose duration is data-dependent (a `while`/`while
/// let` loop's own trip count isn't known until runtime, and neither is
/// how long a `sync`/`race`d `spawn` callee takes), `Some(n)` otherwise:
/// exactly `split_into_segments`'s own segment count, since every OTHER
/// segment boundary (`tick`) is a fixed, statically-known single cycle.
/// Reuses `split_into_segments`/`find_while_anywhere`/`find_spawn_
/// anywhere` directly rather than re-deriving any of this independently
/// -- this project has a standing lesson about exactly that kind of
/// drift (`const_fold` vs `const_eval` silently diverging into a real
/// soundness hole, DESIGN.md's stage-3 history).
///
/// Deliberately does NOT reuse `plan_rule`'s own v0-restriction checks
/// (nested `tick`/`spawn`/`while`, misplaced `break`, ...) -- this is an
/// informational query for hover/`--explain-schedule`, run on ordinary
/// source that may not even be `<sequences>`-tagged yet (mid-edit in the
/// LSP) or may already be known-invalid (`plan_rule`'s own error would
/// fire elsewhere); a best-effort `None` on anything it can't cleanly
/// answer is correct here, not a hard error.
pub(crate) fn sequences_cycle_count(ast: &Ast, body: &[StmtId]) -> Option<u32> {
    if find_while_anywhere(ast, body).is_some() || find_spawn_anywhere(ast, body).is_some() {
        return None;
    }
    Some(split_into_segments(ast, body).len() as u32)
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
            message: "`spawn` needs a direct function call; use `spawn Foo(a, b)`".to_string(),
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
        ret,
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
    if let Some(s) = find_nested_while(ast, &callee_body) {
        return Err(vec![LowerError {
            span: s,
            message: "`while` must be at the top level of a spawned fn's body, not nested in \
                      if/while (v0 restriction, same as `tick`)"
                .to_string(),
        }]);
    }
    if let Some((s, message)) = find_break_misplaced(ast, &callee_body, false) {
        return Err(vec![LowerError {
            span: s,
            message: message.to_string(),
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

    // Prefer the fn's own DECLARED return type over `return_expr`'s
    // inferred type: growing addition means an un-narrowed return value
    // (`return x + 10`, no `trunc`/`.!`) infers a WIDER type than the
    // signature promises (`x + 10` is `[9]` when `x` is `[8]`, even
    // though `Race`'s own `: [8]` return annotation is what callers
    // actually see and what `.result`'s register below must match) --
    // reading `expr_tys[return_expr]` here would size the register to
    // the grown width, then a downstream consumer built against the
    // DECLARED `[8]` (e.g. `__race_value` feeding an `[8]` output) sees
    // a width mismatch. `bits_width_expr`/`const_eval_expr` are the
    // same free functions `firrtl`'s own emission-time resolver uses
    // (see their doc comments) specifically so this isn't a second,
    // independently-drifting width evaluator; falls back to the return
    // expression's own inferred type when the declared return type isn't
    // a plain `[N]` (no declared return type, or a shape these two
    // functions don't resolve, e.g. one depending on an implicit param
    // this call site hasn't solved).
    let declared_result_ty = ret.and_then(|r| {
        let w = crate::types::bits_width_expr(ast, res, r)?;
        let width = crate::types::const_eval_expr(ast, res, w, &HashMap::new())?;
        Some(Ty::Bits(Width::Known(width)))
    });
    let result_ty = declared_result_ty.unwrap_or_else(|| {
        types
            .expr_tys
            .get(&return_expr)
            .cloned()
            .unwrap_or(Ty::Unknown)
    });
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

    // A local written inside a `while` loop's own segment — an
    // accumulator (`acc := acc + n`) or anything else surviving past the
    // loop — needs genuinely different capture semantics than an
    // ordinary tick-crossing value: multiple writes (one per iteration)
    // to the SAME segment index, plus a same-segment self-referential
    // read that's semantically sound (a register read sees last cycle's
    // value) but indistinguishable, from `assign_segs`/`read_segs`
    // alone, from the read-before-write hazard the checks below exist to
    // catch. Not attempted this pass (DESIGN.md's "`while`: multi-cycle
    // loops" section) — a `while` loop may only write module state
    // (regs/mem/outputs) directly, never a local that would need
    // capturing. The checks below are unchanged; only their error
    // messages are sharpened when a `while` segment is involved, so this
    // restriction reads as an intentional boundary instead of the
    // generic capture-machinery text.
    let while_segments: HashSet<usize> = segments
        .iter()
        .filter(|s| s.is_while_loop)
        .map(|s| s.index as usize)
        .collect();

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
        let touches_while = touched.iter().any(|s| while_segments.contains(s));
        if assign_segs.len() != 1 {
            let segs: Vec<String> = assign_segs.iter().map(|s| s.to_string()).collect();
            let message = if touches_while {
                format!(
                    "`{name}` is written across a `while` loop boundary ({}); a value \
                     carried across `while` iterations, or surviving past the loop, isn't \
                     supported yet (v0 restriction: a `while` loop may only write module \
                     state — a reg/mem/output — directly, not a local)",
                    segs.join(", ")
                )
            } else {
                format!(
                    "`{name}` is assigned in multiple segments ({}); sequences lowering \
                     requires a single assignment per captured value (v0 restriction)",
                    segs.join(", ")
                )
            };
            errors.push(LowerError {
                span: res.def(*def).span.clone(),
                message,
            });
            continue;
        }
        let assign_segment = *assign_segs.iter().next().unwrap();
        if let Some(&bad) = read_segs.iter().find(|&&s| s <= assign_segment) {
            let message = if while_segments.contains(&assign_segment) {
                format!(
                    "`{name}` is read in segment {bad}, at or before its assignment inside a \
                     `while` loop's own segment ({assign_segment}); a value carried across \
                     `while` iterations, or surviving past the loop, isn't supported yet (v0 \
                     restriction: a `while` loop may only write module state — a reg/mem/\
                     output — directly, not a local)"
                )
            } else {
                format!(
                    "`{name}` is read in segment {bad} at or before its assignment in \
                     segment {assign_segment}; a captured value must be write-once and \
                     read only in later segments (v0 restriction: promoting it to a \
                     register would change same-cycle read-after-write semantics)"
                )
            };
            errors.push(LowerError {
                span: res.def(*def).span.clone(),
                message,
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
            Stmt::WhileLet { body, .. } => {
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
            // Self-caught while extending this function family for
            // `while`: missing before now, so a `tick` nested inside an
            // `if let` silently escaped this check (see `find_returns`'s
            // own doc comment on this same gap, found the same way).
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
            Stmt::IfLet {
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
            Stmt::WhileLet { body, .. } => {
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
            Stmt::IfLet {
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
            Stmt::WhileLet { body, .. } => {
                if let Some(span) = find_spawn_anywhere(ast, body) {
                    return Some(span);
                }
            }
            _ => {}
        }
    }
    None
}

/// Same shape as `find_nested_tick`, for `while` itself: a v0-scope
/// restriction matching `tick`/`spawn`'s own — `while` must sit at a
/// sequences rule's (or a spawned fn body's) top level, not nested
/// inside `if`/`if let`/another `while`. Segment-cutting
/// (`split_into_segments`) only ever looks at the top level for exactly
/// this reason: a nested `while` would otherwise silently fold into
/// whichever segment it landed in as ordinary, un-lowered text.
fn find_nested_while(ast: &Ast, stmts: &[StmtId]) -> Option<Span> {
    for stmt in stmts {
        match ast.stmt(*stmt) {
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
            Stmt::While { body, .. } => {
                if let Some(span) = find_while_anywhere(ast, body) {
                    return Some(span);
                }
            }
            Stmt::WhileLet { body, .. } => {
                if let Some(span) = find_while_anywhere(ast, body) {
                    return Some(span);
                }
            }
            _ => {}
        }
    }
    None
}

/// `break` may only sit in TAIL position within a `while`/`while let`'s
/// own loop body: as the body's own last statement, or nested inside an
/// `if`/`if let` (however deep, as long as EVERY enclosing level is
/// itself in tail position — Verse's canonical `if (Cond[]) { ... } else
/// { break }` idiom, DESIGN.md's "`while`: multi-cycle loops") as the
/// last statement of a `then`/`else` branch. `render_loop_body` (used by
/// `render_rule`/`render_spawn_segments`) recurses the identical way, so
/// every position this accepts is one it can actually render — a
/// position accepted here that render couldn't handle would silently
/// drop the `break`'s effect rather than erroring on it, exactly the
/// failure mode this whole file's other `find_*` checks exist to close
/// off. `in_loop_tail`: whether `stmts` is itself sitting in an
/// already-tail-eligible position relative to SOME enclosing loop --
/// `false` at the top of a rule/callee body (no loop yet), `true` when
/// recursing into a `While`/`WhileLet`'s own body, and (for an `If`/
/// `IfLet`'s branches) inherited only when the `If`/`IfLet` itself is
/// both already tail-eligible AND the last statement of its own list.
fn find_break_misplaced(
    ast: &Ast,
    stmts: &[StmtId],
    in_loop_tail: bool,
) -> Option<(Span, &'static str)> {
    let last_idx = stmts.len().checked_sub(1);
    for (i, stmt) in stmts.iter().enumerate() {
        let is_last = Some(i) == last_idx;
        match ast.stmt(*stmt) {
            Stmt::Break => {
                let span = ast.stmt_spans[stmt.0 as usize].clone();
                if !in_loop_tail {
                    return Some((
                        span,
                        "`break` may only appear as the last statement of a `while`/`while \
                         let`'s own body, or of a `then`/`else` branch nested directly in \
                         one -- either it isn't inside a loop at all, or an enclosing `if`/ \
                         `if let` isn't itself in that tail position (v0 restriction)",
                    ));
                }
                if !is_last {
                    return Some((
                        span,
                        "`break` must be the last statement of its own block -- a `while`'s \
                         own body, or a `then`/`else` branch nested directly in one (v0 \
                         restriction: nothing may follow it)",
                    ));
                }
            }
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                let branch_tail = in_loop_tail && is_last;
                if let Some(v) = find_break_misplaced(ast, then_body, branch_tail) {
                    return Some(v);
                }
                if let Some(b) = else_body
                    && let Some(v) = find_break_misplaced(ast, b, branch_tail)
                {
                    return Some(v);
                }
            }
            Stmt::IfLet {
                then_body,
                else_body,
                ..
            } => {
                let branch_tail = in_loop_tail && is_last;
                if let Some(v) = find_break_misplaced(ast, then_body, branch_tail) {
                    return Some(v);
                }
                if let Some(b) = else_body
                    && let Some(v) = find_break_misplaced(ast, b, branch_tail)
                {
                    return Some(v);
                }
            }
            Stmt::While { body, .. } => {
                if let Some(v) = find_break_misplaced(ast, body, true) {
                    return Some(v);
                }
            }
            Stmt::WhileLet { body, .. } => {
                if let Some(v) = find_break_misplaced(ast, body, true) {
                    return Some(v);
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
            Stmt::IfLet {
                then_body,
                else_body,
                ..
            } => find_unsupported_construct(ast, res, then_body, false).or_else(|| {
                else_body
                    .as_deref()
                    .and_then(|b| find_unsupported_construct(ast, res, b, false))
            }),
            Stmt::While { body, .. } => find_unsupported_construct(ast, res, body, false),
            Stmt::WhileLet { body, .. } => find_unsupported_construct(ast, res, body, false),
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

/// Top-level expressions directly reachable from one statement (not
/// recursing into nested statement bodies; the caller handles that).
fn stmt_exprs(ast: &Ast, id: StmtId) -> Vec<ExprId> {
    match ast.stmt(id).clone() {
        Stmt::Expr(e) => vec![e],
        Stmt::Assign { lhs, rhs } => vec![lhs, rhs],
        Stmt::Let { init, .. } => vec![init],
        Stmt::Tick => vec![],
        Stmt::Break => vec![],
        Stmt::Return(e) => e.into_iter().collect(),
        Stmt::If { cond, .. } => vec![cond],
        Stmt::IfLet { init, .. } => vec![init],
        Stmt::While { cond, .. } => vec![cond],
        Stmt::WhileLet { init, .. } => vec![init],
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
            Stmt::Break => {}
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
            // Same exemption `IfLet`'s arm above has, for the identical
            // reason: `name` isn't registered in `assigns`/`let_bound`
            // (no rewrite exists for `while let`'s surrounding loop
            // structure either), and `init` is scanned for reads only.
            // `body` scans at the SAME segment index as this `while
            // let` itself, same as `While`'s own arm — the loop's own
            // body executes as part of this one self-looping segment.
            Stmt::WhileLet { init, body, .. } => {
                scan_expr(ast, res, init, segment, reads);
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
            // Self-caught while extending this same function for
            // `WhileLet`: this arm was missing for `IfLet` too, a real
            // pre-existing gap (not just an unreachable defensive case
            // the others in this file mostly are) — a captured param
            // referenced inside `if let`'s own branches, in a spawned
            // callee, never got renamed to its private register name,
            // since nothing recursed into `then_body`/`else_body` to
            // find that reference at all. Confirmed live by direct
            // probe: silently left the ORIGINAL param name in the
            // rendered text, which then failed to resolve on the
            // second pass (`emit_from_source`'s own resolve-errors
            // assert) rather than compiling wrong — a hard failure, not
            // a silent miscompile, but a real gap all the same.
            Stmt::IfLet {
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
            Stmt::WhileLet { body, .. } => {
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
