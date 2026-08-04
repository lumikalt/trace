//! Phase 2 ("render"): splices generated trace source text over each
//! lowered rule's original span — save registers, a continuation
//! register, and one plain single-cycle rule per segment (`render_rule`,
//! `render_spawn_regs`/`render_spawn_trigger`/`render_spawn_segments` for
//! a `spawn` occurrence's own private registers/rules) — plus a
//! `schedule` directive naming a lowered rule (`rewrite_schedules`). See
//! mod.rs's module doc comment for the whole pass's design story,
//! including why a plain 0 reset is sound for every continuation
//! register.

use super::{LoweredRule, SpawnPlan, find_break_anywhere};
use crate::ast::{Ast, Expr, Item, ItemId, Stmt, StmtId};
use crate::lexer::Span;
use crate::resolve::DefId;
use std::collections::HashMap;

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

pub(crate) fn rewrite_schedules(
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
/// An `is_while_loop` segment's own `if` header text — `if COND {` for
/// `Stmt::While`, `if let NAME = EXPR {` for `Stmt::WhileLet` — alongside
/// the loop's own body. Shared between `render_rule` and `render_spawn_
/// segments`, the two places a self-looping segment gets rendered:
/// `while let`'s header reuses `if let`'s OWN existing syntax verbatim,
/// so the rendered text re-enters the pipeline as an ordinary `if let`
/// and gets its mux synthesis, `if_let_binds`, and every write-threading
/// arm for free — the identical "reuse `if`'s machinery" move plain
/// `while` already makes (DESIGN.md's "`while` lowering").
fn while_loop_header<'a>(
    ast: &'a Ast,
    src: &str,
    stmt: StmtId,
    rewrites: &[(Span, String)],
) -> (String, &'a [StmtId]) {
    match ast.stmt(stmt) {
        Stmt::While { cond, body } => {
            let cond_span = ast.expr_spans[cond.0 as usize].clone();
            (
                format!("if {} {{\n", splice(src, &cond_span, rewrites).trim_end()),
                body.as_slice(),
            )
        }
        Stmt::WhileLet { name, init, body } => {
            let init_span = ast.expr_spans[init.0 as usize].clone();
            (
                format!(
                    "if let {name} = {} {{\n",
                    splice(src, &init_span, rewrites).trim_end()
                ),
                body.as_slice(),
            )
        }
        _ => unreachable!("an is_while_loop segment's own statement is always While/WhileLet"),
    }
}

/// Renders `stmts` — a `while`/`while let`'s own body, or a `then`/
/// `else` branch nested directly within one — into `out`. When NO
/// `break` is reachable anywhere in `stmts` (`find_break_anywhere`),
/// this reproduces EXACTLY the pre-`break` behavior byte for byte: every
/// statement, including the last, spliced verbatim, then `{cont} :=
/// {stay}` appended once at the end — the common case, and the ONLY
/// case before `break` existed, left completely undisturbed rather than
/// unconditionally restructured into an equivalent-but-more-deeply-
/// nested form.
///
/// When a `break` IS reachable, every statement except the list's own
/// last one still splices verbatim; the last one gets special handling
/// based on its shape. A trailing `break` is OMITTED — nothing of it
/// reaches the rendered text — and `{cont} := {advance}` is appended
/// instead (exits the loop this cycle). A trailing `if`/`if let`
/// recurses into EACH of its own branches with this SAME function,
/// synthesizing an empty branch (just `{cont} := {stay}`) when no `else`
/// is written — so a `break` buried arbitrarily deep in tail position
/// (Verse's own `if (Cond[]) { ... } else { break }` idiom, nested as
/// deep as the user likes as long as every enclosing level stays in
/// tail position) renders correctly at every level, not just one hop
/// in. Any OTHER trailing statement (no break in this branch's own
/// subtree, but a sibling branch elsewhere in the same tail `if` does
/// have one) splices verbatim too, then `{cont} := {stay}`, same as the
/// no-break case above — just emitted one level deeper, inside this
/// branch rather than after the whole `if` closes.
///
/// `find_break_misplaced` (this file) validates every `break` reachable
/// from a loop's body sits in exactly one of the positions this
/// function knows how to render — kept in sync deliberately: a shape
/// accepted there that this function couldn't render would silently
/// drop the `break`'s effect rather than erroring on it, exactly the
/// failure mode this whole file's `find_*` checks exist to close off.
#[allow(clippy::too_many_arguments)]
fn render_loop_body(
    out: &mut String,
    ast: &Ast,
    src: &str,
    stmts: &[StmtId],
    rewrites: &[(Span, String)],
    cont_name: &str,
    stay: u64,
    advance: u64,
) {
    if find_break_anywhere(ast, stmts).is_none() {
        for stmt in stmts {
            let span = ast.stmt_spans[stmt.0 as usize].clone();
            out.push_str("    ");
            out.push_str(&splice(src, &span, rewrites));
        }
        out.push_str(&format!("        {cont_name} := {stay}\n"));
        return;
    }
    // `stmts` is non-empty here: `find_break_anywhere` only ever returns
    // `Some` by finding a `Stmt::Break` inside it, which requires at
    // least one statement to exist.
    let (last, init) = stmts.split_last().expect("break implies a statement");
    for stmt in init {
        let span = ast.stmt_spans[stmt.0 as usize].clone();
        out.push_str("    ");
        out.push_str(&splice(src, &span, rewrites));
    }
    match ast.stmt(*last).clone() {
        Stmt::Break => {
            out.push_str(&format!("        {cont_name} := {advance}\n"));
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            let cond_span = ast.expr_spans[cond.0 as usize].clone();
            out.push_str(&format!(
                "        if {} {{\n",
                splice(src, &cond_span, rewrites).trim_end()
            ));
            render_loop_body(
                out, ast, src, &then_body, rewrites, cont_name, stay, advance,
            );
            out.push_str("        } else {\n");
            render_loop_body(
                out,
                ast,
                src,
                &else_body.unwrap_or_default(),
                rewrites,
                cont_name,
                stay,
                advance,
            );
            out.push_str("        }\n");
        }
        Stmt::IfLet {
            name,
            init,
            then_body,
            else_body,
        } => {
            let init_span = ast.expr_spans[init.0 as usize].clone();
            out.push_str(&format!(
                "        if let {name} = {} {{\n",
                splice(src, &init_span, rewrites).trim_end()
            ));
            render_loop_body(
                out, ast, src, &then_body, rewrites, cont_name, stay, advance,
            );
            out.push_str("        } else {\n");
            render_loop_body(
                out,
                ast,
                src,
                &else_body.unwrap_or_default(),
                rewrites,
                cont_name,
                stay,
                advance,
            );
            out.push_str("        }\n");
        }
        _ => {
            let span = ast.stmt_spans[last.0 as usize].clone();
            out.push_str("    ");
            out.push_str(&splice(src, &span, rewrites));
            out.push_str(&format!("        {cont_name} := {stay}\n"));
        }
    }
}

pub(crate) fn render_rule(ast: &Ast, src: &str, lr: &LoweredRule) -> String {
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
        let next = if seg.index + 1 < nsegs {
            seg.index + 1
        } else {
            0
        };
        if seg.is_while_loop {
            // `seg.stmts` is exactly `[the Stmt::While/WhileLet itself]`
            // (see `Segment::is_while_loop`'s own doc comment) --
            // `find_nested_spawn`/`find_unsupported_construct` (called
            // before this rule was ever planned) already confirmed no
            // spawn trigger/sync/race hides inside a loop's body, so --
            // unlike the ordinary segment loop below -- every statement
            // here is plain splice (`render_loop_body`'s own recursion
            // into a tail `if`/`if let` handles `break`; nothing else
            // needs per-shape dispatch).
            let (header, body) =
                while_loop_header(ast, src, seg.stmts[0], &lr.handle_field_rewrites);
            out.push_str("    ");
            out.push_str(&header);
            render_loop_body(
                &mut out,
                ast,
                src,
                body,
                &lr.handle_field_rewrites,
                &lr.cont_name,
                seg.index,
                next,
            );
            out.push_str("    } else {\n");
            out.push_str(&format!("        {} := {next}\n", lr.cont_name));
            out.push_str("    }\n");
            out.push_str("}\n\n");
            continue;
        }
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
        if seg.is_while_loop {
            // Mirrors `render_rule`'s own `is_while_loop` branch exactly
            // — see its doc comment and `while_loop_header`'s. `is_last`
            // is always false here: a `return` can never be nested
            // inside a loop's body (`find_returns`'s early-return
            // rejection in `plan_spawn` already confirmed that), so a
            // while segment is never the spawned fn's own final segment.
            let (header, body) = while_loop_header(ast, src, seg.stmts[0], &spawn.rename_edits);
            out.push_str("    ");
            out.push_str(&header);
            render_loop_body(
                out,
                ast,
                src,
                body,
                &spawn.rename_edits,
                &spawn.cont_name,
                seg.index,
                seg.index + 1,
            );
            out.push_str("    } else {\n");
            out.push_str(&format!(
                "        {} := {}\n",
                spawn.cont_name,
                seg.index + 1
            ));
            out.push_str("    }\n");
            out.push_str("}\n\n");
            continue;
        }
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
