//! FIRRTL text emission (FIRRTL version 4.0.0, `firtool`-compatible).
//! Front end only, per DESIGN.md: no backend, emit an existing IR as
//! text. This pass turns the scheduler's derived conflict/urgency data
//! into the "guarded atomic rule -> plain synchronous hardware" step —
//! the payoff of the whole design: nobody writes a mux or a `when`
//! chain by hand.
//!
//! Scope, v0. Each is an explicit error, not a silent skip:
//! - A `module` may be declared inside another module's body — purely a
//!   naming convenience (resolve.rs scopes it to its parent, invisible to
//!   siblings), not a hardware relationship. Composition is still only by
//!   name via `inst child : Child` (`child.port` reads/writes one of its
//!   ports), and FIRRTL itself has no nested-module concept: a lexically
//!   nested module still emits as its own top-level FIRRTL block, found
//!   by `all_modules` regardless of nesting depth. Exactly one module in
//!   the whole file must be uninstantiated (the "top"); the rest must be
//!   reachable from it via `inst`, with no cycle. Modules share no state
//!   with each other regardless of nesting — a rule may only reference
//!   `reg`/`mem`/`fifo`/`input`/`output`/`inst` declared in its OWN
//!   module, an explicit resolve-time error otherwise (resolve.rs's
//!   `check_module_boundary`), since a name from a different module has
//!   no counterpart in the emitted FIRRTL text this pass produces for
//!   the module actually being compiled.
//! - Each instance port is its own conflict resource (resolve.rs
//!   synthesizes one per `(inst, port)` pair): two rules touching
//!   different ports of the same instance don't conflict, unlike a `mem`
//!   array's still-whole-array conservative model — a port name is
//!   static/lexical, so no runtime disjointness proof is needed to tell
//!   two ports apart. A port write may nest in `if`/`else` (threaded
//!   through a `mux`, same as a register write).
//! - A call to a user `fn`/`impl` is inlined at its call site (FIRRTL has
//!   no call concept). Restricted to a callee whose body is zero or more
//!   `let` bindings then either a trailing `return <expr>` or an
//!   `if`/`else` whose branches both recurse into that same shape
//!   (mandatory `else`, folded into a `mux`) — no state writes,
//!   guards/fifo ops, or further calls anywhere (which also rules out
//!   recursion: a callee that cannot call anything can never call
//!   itself). A `spec` call cannot reach this pass at all (effects.rs
//!   already rejects it outside spec-only code); a builtin call (e.g.
//!   `prio`) is a separate, still-unsupported gap.
//! - No `<sequences>` rules: run `lower::plan`/`render` first. This pass
//!   only lowers "guarded atomic rule" to hardware, not "cycle-crossing
//!   rule" to guarded atomic rules — that is `lower`'s job.
//! - A rule's guards (`expr?`) and fifo operations (`Enq[x]`/`Deq[]`)
//!   must all appear before any state write, and not nested in
//!   `if`/`while`: DESIGN.md's failure = abort-the-whole-rule only
//!   lowers to "AND every failure condition into one `when`" when no
//!   write could have already committed before a failure is checked.
//! - Expression surface: identifiers, integer literals, arithmetic
//!   (`+`/`-`/`*`, all modular/width-matching the type checker),
//!   bitwise (`&`/`|`/`^`/`~`), static (literal-amount only) shifts
//!   (`<<`/`>>`), unary negate (`-`), comparisons, bit-select/slice
//!   (`x[i]`/`x[hi..lo]`, literal bounds only), memory indexing,
//!   `instance.port`, and a call to a simple user `fn`/`impl` (see
//!   above). No other field access, `/`/`%`, dynamic-amount shifts,
//!   computed bit-select bounds, or logical `!` yet.
//!
//! Fifos are depth-1 buffers: one data register plus one valid bit.
//! `Deq[]` succeeds iff valid; `Enq[x]` succeeds iff not valid — the
//! two failure conditions fold into the rule's guard exactly like an
//! explicit `?`. A rule may not both `Enq` and `Deq` the same fifo:
//! that would require valid=1 and valid=0 at once, an always-false
//! guard, so it is rejected explicitly rather than silently synthesized
//! as permanently dead hardware.
//!
//! Memories get one reader port per static read site (not one shared
//! port): the scheduler treats read-read as free, which is only sound
//! in hardware if reads never contend for a port. Writers share one
//! port, since the scheduler already serializes all writers (and every
//! writer conflicts with every reader of the same array in v0), so
//! `read-under-write` is never exercised — it is set to `undefined`.

use crate::ast::{Ast, BinOp, Expr, ExprId, Item, ItemId, Stmt, StmtId, UnOp};
use crate::effects::Effects;
use crate::lexer::Span;
use crate::resolve::{DefId, DefKind, Resolution};
use crate::schedule::Schedule;
use crate::types::{Ty, Types, Width};
use std::collections::HashMap;
use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmitError {
    pub span: Span,
    pub message: String,
}

pub fn emit(
    ast: &Ast,
    res: &Resolution,
    fx: &Effects,
    types: &Types,
    sched: &Schedule,
) -> Result<String, Vec<EmitError>> {
    let modules: Vec<ItemId> = all_modules(ast);
    if modules.is_empty() {
        return Err(vec![EmitError {
            span: 0..0,
            message: "FIRRTL emission needs at least one module in the file".to_string(),
        }]);
    }

    // A top-level module's own `DefId` -> its `ItemId`, so an `inst`'s
    // resolved target def can be turned back into the module item it names.
    let item_of_module_def: HashMap<DefId, ItemId> = modules
        .iter()
        .filter_map(|id| res.item_defs.get(id).map(|d| (*d, *id)))
        .collect();

    // "The top" is whichever top-level module nobody instantiates. A file
    // with no `inst` at all still works exactly as before: with zero
    // instantiation edges, every module is a candidate, so exactly one
    // module means exactly one candidate.
    let mut instantiated: std::collections::HashSet<ItemId> = std::collections::HashSet::new();
    for &m in &modules {
        instantiated.extend(inst_targets(ast, res, m, &item_of_module_def));
    }
    let tops: Vec<ItemId> = modules
        .iter()
        .copied()
        .filter(|m| !instantiated.contains(m))
        .collect();
    let top = match tops.as_slice() {
        [top] => *top,
        [] => {
            return Err(vec![EmitError {
                span: 0..0,
                message: "FIRRTL emission needs exactly one top module (one that no \
                          other module instantiates), but every module here is \
                          instantiated by another; that means a cycle (a module \
                          cannot instantiate itself, even indirectly)"
                    .to_string(),
            }]);
        }
        _ => {
            let names: Vec<&str> = tops.iter().map(|m| module_name(ast, *m)).collect();
            return Err(vec![EmitError {
                span: 0..0,
                message: format!(
                    "FIRRTL emission needs exactly one top module (one that no other \
                     module instantiates); found {} unrelated candidates: {} (v0 \
                     restriction — instantiate one from another with `inst`, or \
                     remove the ones you don't need)",
                    tops.len(),
                    names.join(", ")
                ),
            }]);
        }
    };

    let to_emit = match transitive_modules(ast, res, top, &item_of_module_def) {
        Ok(order) => order,
        Err(e) => return Err(vec![e]),
    };

    let mut blocks = Vec::new();
    let mut all_errors = Vec::new();
    for &m in &to_emit {
        match emit_module(ast, res, fx, types, sched, m, m == top, &item_of_module_def) {
            Ok(text) => blocks.push(text),
            Err(errs) => all_errors.extend(errs),
        }
    }
    if !all_errors.is_empty() {
        return Err(all_errors);
    }

    let mut out = String::new();
    let _ = writeln!(out, "FIRRTL version 4.0.0");
    let _ = writeln!(out, "circuit {} :", module_name(ast, top));
    for block in blocks {
        out.push_str(&block);
        out.push('\n');
    }
    Ok(out)
}

/// Every `Item::Module` in the file, regardless of lexical nesting depth
/// — a module declared inside another module's body (visible only within
/// that scope, per resolve.rs) still gets its own top-level FIRRTL block,
/// same as a module declared at the file's own top level; FIRRTL itself
/// has no nested-module concept, only cross-references via `inst X of Y`.
fn all_modules(ast: &Ast) -> Vec<ItemId> {
    let mut out = Vec::new();
    let mut worklist: Vec<ItemId> = ast.roots.clone();
    while let Some(id) = worklist.pop() {
        if let Item::Module { items, .. } = ast.item(id) {
            out.push(id);
            worklist.extend(items.iter().copied());
        }
    }
    out
}

/// The modules a module `m` directly instantiates via `inst`.
fn inst_targets(
    ast: &Ast,
    res: &Resolution,
    m: ItemId,
    item_of_module_def: &HashMap<DefId, ItemId>,
) -> Vec<ItemId> {
    let Item::Module { items, .. } = ast.item(m) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|it| match ast.item(*it) {
            Item::Inst { module, .. } => res
                .expr_defs
                .get(module)
                .and_then(|d| item_of_module_def.get(d))
                .copied(),
            _ => None,
        })
        .collect()
}

/// Every module reachable from `top` via `inst`, `top` included. A cycle
/// in the instantiation graph is an error — a real hardware hierarchy
/// cannot contain itself, even indirectly.
fn transitive_modules(
    ast: &Ast,
    res: &Resolution,
    top: ItemId,
    item_of_module_def: &HashMap<DefId, ItemId>,
) -> Result<Vec<ItemId>, EmitError> {
    let mut order = Vec::new();
    let mut done: std::collections::HashSet<ItemId> = std::collections::HashSet::new();
    let mut on_path: Vec<ItemId> = Vec::new();
    visit_module(
        ast,
        res,
        top,
        item_of_module_def,
        &mut order,
        &mut done,
        &mut on_path,
    )?;
    Ok(order)
}

fn visit_module(
    ast: &Ast,
    res: &Resolution,
    m: ItemId,
    item_of_module_def: &HashMap<DefId, ItemId>,
    order: &mut Vec<ItemId>,
    done: &mut std::collections::HashSet<ItemId>,
    on_path: &mut Vec<ItemId>,
) -> Result<(), EmitError> {
    if on_path.contains(&m) {
        return Err(EmitError {
            span: ast.item_spans[m.0 as usize].clone(),
            message: format!(
                "module instantiation forms a cycle at `{}`: a module cannot \
                 instantiate itself, even indirectly",
                module_name(ast, m)
            ),
        });
    }
    if !done.insert(m) {
        return Ok(());
    }
    on_path.push(m);
    for target in inst_targets(ast, res, m, item_of_module_def) {
        visit_module(ast, res, target, item_of_module_def, order, done, on_path)?;
    }
    on_path.pop();
    order.push(m);
    Ok(())
}

fn module_name(ast: &Ast, m: ItemId) -> &str {
    match ast.item(m) {
        Item::Module { name, .. } => &name.text,
        _ => "?",
    }
}

/// Emit one module's FIRRTL text (its `public module`/`module` line, ports,
/// declarations, and body) — everything except the `circuit` wrapper,
/// which the caller writes once for the whole file.
fn emit_module(
    ast: &Ast,
    res: &Resolution,
    fx: &Effects,
    types: &Types,
    sched: &Schedule,
    module: ItemId,
    is_public: bool,
    item_of_module_def: &HashMap<DefId, ItemId>,
) -> Result<String, Vec<EmitError>> {
    let Item::Module {
        name: mod_name,
        items,
    } = ast.item(module).clone()
    else {
        unreachable!()
    };

    let mut cx = Emitter {
        ast,
        res,
        fx,
        types,
        errors: Vec::new(),
        read_ports: HashMap::new(),
        output_regs: HashMap::new(),
        locals: HashMap::new(),
    };

    // `(match_name, emit_name, width, init)`. For a plain `reg`, both
    // names are the user's; for an `output`, `match_name` is the port
    // name (what rule bodies write) and `emit_name` is its internal
    // backing register (see the `Item::Output` arm below).
    let mut regs: Vec<(String, String, u64, u64)> = Vec::new();
    let mut mems = Vec::new();
    // `(fifo_name, width)`, depth-1 buffers (see module doc comment).
    let mut fifos: Vec<(String, u64)> = Vec::new();
    // `(port_name, width)`.
    let mut inputs: Vec<(String, u64)> = Vec::new();
    // `(port_name, internal_reg_name, width)`.
    let mut outputs: Vec<(String, String, u64)> = Vec::new();
    // `(inst_name, target_module's_firrtl_name, inst_def)`.
    let mut instances: Vec<(String, String, DefId)> = Vec::new();
    let mut rules: Vec<ItemId> = Vec::new();
    for id in &items {
        match ast.item(*id) {
            Item::Reg { name, .. } => {
                let def = res.item_defs[id];
                let Some(Ty::Bits(Width::Known(w))) = cx.state_width(def) else {
                    cx.error(
                        ast.item_spans[id.0 as usize].clone(),
                        format!("`{}` has no concrete bit width", name.text),
                    );
                    continue;
                };
                let init = match ast.item(*id) {
                    Item::Reg { init: Some(e), .. } => cx.const_eval(*e).unwrap_or(0),
                    _ => 0,
                };
                regs.push((name.text.clone(), name.text.clone(), w, init));
            }
            Item::Input { name, .. } => {
                let def = res.item_defs[id];
                let Some(Ty::Bits(Width::Known(w))) = cx.state_width(def) else {
                    cx.error(
                        ast.item_spans[id.0 as usize].clone(),
                        format!("`{}` has no concrete bit width", name.text),
                    );
                    continue;
                };
                inputs.push((name.text.clone(), w));
            }
            Item::Output { name, .. } => {
                let def = res.item_defs[id];
                let Some(Ty::Bits(Width::Known(w))) = cx.state_width(def) else {
                    cx.error(
                        ast.item_spans[id.0 as usize].clone(),
                        format!("`{}` has no concrete bit width", name.text),
                    );
                    continue;
                };
                let init = match ast.item(*id) {
                    Item::Output { init: Some(e), .. } => cx.const_eval(*e).unwrap_or(0),
                    _ => 0,
                };
                // A rule-visible output is register-backed: driving it
                // combinationally would expose a rule's speculative,
                // pre-commit value, which breaks the "writes are
                // speculative until the clock edge" invariant the whole
                // scheduler is built on. So `output x` is really an
                // ordinary register (`__out_x`) wired out to a port.
                let internal = format!("__out_{}", name.text);
                regs.push((name.text.clone(), internal.clone(), w, init));
                outputs.push((name.text.clone(), internal.clone(), w));
                cx.output_regs.insert(name.text.clone(), internal);
            }
            Item::Mem { name, .. } => {
                let def = res.item_defs[id];
                let Some(Ty::Mem { elem, len }) = cx.state_mem_ty(def) else {
                    cx.error(
                        ast.item_spans[id.0 as usize].clone(),
                        format!("`{}` has no concrete memory type", name.text),
                    );
                    continue;
                };
                let Ty::Bits(Width::Known(w)) = *elem else {
                    cx.error(
                        ast.item_spans[id.0 as usize].clone(),
                        format!("`{}`'s element type has no concrete width", name.text),
                    );
                    continue;
                };
                mems.push((name.text.clone(), w, len));
            }
            Item::Fifo { name, .. } => {
                let def = res.item_defs[id];
                let Some(Ty::Fifo(elem)) = cx.state_width(def) else {
                    cx.error(
                        ast.item_spans[id.0 as usize].clone(),
                        format!("`{}` has no concrete fifo type", name.text),
                    );
                    continue;
                };
                let Ty::Bits(Width::Known(w)) = *elem else {
                    cx.error(
                        ast.item_spans[id.0 as usize].clone(),
                        format!("`{}`'s element type has no concrete width", name.text),
                    );
                    continue;
                };
                // A depth-1 buffer: one data register, one valid bit,
                // both internally named to avoid colliding with a user
                // identifier (same `__`-prefix convention as `__out_x`
                // and lower.rs's `__cont_x`).
                regs.push((
                    fifo_valid_name(&name.text),
                    fifo_valid_name(&name.text),
                    1,
                    0,
                ));
                regs.push((fifo_data_name(&name.text), fifo_data_name(&name.text), w, 0));
                fifos.push((name.text.clone(), w));
            }
            Item::Rule { name, body, .. } => {
                let still_sequences = fx.sigs.get(id).is_some_and(|s| s.sequences)
                    || body.iter().any(|s| matches!(ast.stmt(*s), Stmt::Tick));
                if still_sequences {
                    cx.error(
                        ast.item_spans[id.0 as usize].clone(),
                        format!(
                            "`{}` is still a <sequences> rule with `tick`; run sequences \
                             lowering first (lower::plan + render), then emit the result",
                            name.text
                        ),
                    );
                    continue;
                }
                rules.push(*id);
            }
            Item::Inst {
                name,
                module: module_expr,
            } => {
                let def = res.item_defs[id];
                let target_item = res
                    .expr_defs
                    .get(module_expr)
                    .and_then(|d| item_of_module_def.get(d));
                let Some(&target_item) = target_item else {
                    // Already reported by resolve.rs (unknown name or not
                    // a module).
                    continue;
                };
                instances.push((
                    name.text.clone(),
                    module_name(ast, target_item).to_string(),
                    def,
                ));
            }
            // A nested module declaration is transparent to its parent's
            // own body: it contributes no state/rules here, only a name
            // scoped to this module (see resolve.rs) that `inst` can
            // target. It gets its own separate FIRRTL module block,
            // discovered and emitted independently (see `all_modules`).
            Item::Fn { .. } | Item::Schedule { .. } | Item::Module { .. } => {}
        }
    }

    // Bridge each output port to its backing register; independent of
    // whether any rule fires this cycle, so it holds even with no rules.
    let mut port_connects = String::new();
    for (port, internal, _) in &outputs {
        let _ = writeln!(port_connects, "    connect {port}, {internal}");
    }

    // A module with no rules at all gets no `GroupSchedule` (schedule.rs
    // skips empty groups); treat that the same as an empty one rather
    // than special-casing it, so a rule-less module with only instances
    // (wired entirely by default connects) still emits correctly.
    let group = sched.groups.iter().find(|g| g.module == Some(module));
    let empty_order: Vec<ItemId> = Vec::new();
    let empty_conflicts: Vec<crate::schedule::Conflict> = Vec::new();
    let (order, conflicts) = match group {
        Some(g) => (&g.order, &g.conflicts),
        None => (&empty_order, &empty_conflicts),
    };

    // Assign one reader port per static mem-read site, across all rules,
    // before compiling any expression (a read site's port name must be
    // known wherever it is later referenced).
    for rule in &rules {
        let body = rule_body(ast, *rule);
        cx.collect_read_sites(&body);
    }

    // Guards must all precede any state write, and not be nested.
    // Memory writes must stay top-level (not threaded through a mux yet).
    // Register writes and instance-port writes may nest in if/else —
    // SUBLEQ's branch does, for a register — both are threaded through a
    // mux (see `reg_value_in_stmts`/`inst_port_value_in_stmts`).
    for rule in &rules {
        cx.check_guard_placement(*rule);
        cx.check_fifo_same_cycle(*rule);
        cx.check_no_reassigned_locals(*rule);
        let body = rule_body(ast, *rule);
        if let Some(span) = find_nested_mem_write(ast, &body) {
            cx.error(
                span,
                "a memory write nested in if/while is not yet supported in FIRRTL \
                 emission (v0 restriction); only a register or instance port write \
                 may be conditional"
                    .to_string(),
            );
        }
    }
    if !cx.errors.is_empty() {
        return Err(cx.errors);
    }

    // fires_i in urgency order: guard AND NOT any higher-urgency,
    // non-exempted, conflicting rule that itself fires.
    let mut fires_name: HashMap<ItemId, String> = HashMap::new();
    let mut fires_body = String::new();
    for (rank, rule) in order.iter().enumerate() {
        let rule_name = item_name(ast, *rule);
        let signal = format!("fires_{rule_name}");
        cx.enter_rule(*rule);
        let guard = cx.compile_guard(*rule);
        let mut expr = guard;
        for conflict in conflicts {
            if conflict.exempted {
                continue;
            }
            let other = if conflict.a == *rule {
                Some(conflict.b)
            } else if conflict.b == *rule {
                Some(conflict.a)
            } else {
                None
            };
            let Some(other) = other else { continue };
            let other_rank = order.iter().position(|r| *r == other).unwrap();
            if other_rank < rank {
                let other_signal = &fires_name[&other];
                expr = format!("and({expr}, not({other_signal}))");
            }
        }
        let _ = writeln!(fires_body, "    node {signal} = {expr}");
        fires_name.insert(*rule, signal);
    }

    // Every mem's `mem` declaration must list all its port names up
    // front (reader/writer are declarations, not just references).
    let mut mem_ports: HashMap<String, (Vec<String>, Option<String>)> = mems
        .iter()
        .map(|(n, _, _)| (n.clone(), (Vec::new(), None)))
        .collect();

    // Read ports: driven unconditionally (reads are free; latency 0).
    let mut mem_body = String::new();
    for (site_expr, port) in cx.read_ports.clone() {
        let Expr::Bracket { callee, args } = ast.expr(site_expr).clone() else {
            continue;
        };
        let addr = cx.compile_expr(args[0]).unwrap_or_default();
        let mem_name = match ast.expr(callee) {
            Expr::Ident(n) => n.clone(),
            _ => continue,
        };
        if let Some((readers, _)) = mem_ports.get_mut(&mem_name) {
            readers.push(port.clone());
        }
        let _ = writeln!(mem_body, "    connect {mem_name}.{port}.clk, clock");
        let _ = writeln!(mem_body, "    connect {mem_name}.{port}.en, UInt<1>(1)");
        let _ = writeln!(mem_body, "    connect {mem_name}.{port}.addr, {addr}");
    }

    // Writers: one shared port per mem, enable = OR of writer fires,
    // priority-muxed by urgency (most urgent's connect emitted last).
    for (mem_name, elem_width, depth) in &mems {
        let writers = writers_of(ast, res, &rules, mem_name);
        if writers.is_empty() {
            continue;
        }
        let port = format!("w_{mem_name}");
        if let Some((_, writer)) = mem_ports.get_mut(mem_name) {
            *writer = Some(port.clone());
        }
        let en = writers
            .iter()
            .map(|r| fires_name[r].clone())
            .reduce(|a, b| format!("or({a}, {b})"))
            .unwrap();
        let addr_w = clog2(*depth).max(1);
        let _ = writeln!(mem_body, "    connect {mem_name}.{port}.clk, clock");
        let _ = writeln!(mem_body, "    connect {mem_name}.{port}.en, {en}");
        let _ = writeln!(mem_body, "    connect {mem_name}.{port}.mask, UInt<1>(1)");
        // FIRRTL requires every mem-port field driven on every path,
        // even when unused (en=0); unlike a `reg`, a port has no
        // automatic "hold". Default to 0, then let the firing writer's
        // `when` below override via last-connect.
        let _ = writeln!(
            mem_body,
            "    connect {mem_name}.{port}.addr, UInt<{addr_w}>(0)"
        );
        let _ = writeln!(
            mem_body,
            "    connect {mem_name}.{port}.data, UInt<{elem_width}>(0)"
        );
        // Least urgent first, so the most urgent's connect wins (FIRRTL
        // last-connect); only one writer's fires can be true at once
        // among non-exempted conflicting writers.
        let mut ordered = writers.clone();
        ordered.sort_by_key(|r| std::cmp::Reverse(order.iter().position(|x| x == r)));
        for rule in ordered {
            cx.enter_rule(rule);
            let (addr, data) = cx.write_target(rule, mem_name, *elem_width);
            let f = &fires_name[&rule];
            let _ = writeln!(mem_body, "    when {f} :");
            let _ = writeln!(mem_body, "      connect {mem_name}.{port}.addr, {addr}");
            let _ = writeln!(mem_body, "      connect {mem_name}.{port}.data, {data}");
        }
    }

    // Fifos: depth-1 buffers. At most one rule can touch a given fifo
    // per cycle — every fifo op reads+writes it (effects.rs), so every
    // touching rule conflicts with every other, exactly like mem
    // writers above; same priority-mux pattern, though only one
    // `when` can ever actually be live per fifo.
    let mut fifo_body = String::new();
    for (fifo_name, width) in &fifos {
        let mut touching: Vec<(ItemId, bool, Option<ExprId>)> = Vec::new();
        for rule in &rules {
            let body = rule_body(ast, *rule);
            if let Some((is_enq, value)) = body.iter().find_map(|s| match cx.fifo_op_stmt(*s) {
                Some((name, is_enq, value)) if &name == fifo_name => Some((is_enq, value)),
                _ => None,
            }) {
                touching.push((*rule, is_enq, value));
            }
        }
        if touching.is_empty() {
            continue;
        }
        touching.sort_by_key(|(r, _, _)| std::cmp::Reverse(order.iter().position(|x| x == r)));
        let valid = fifo_valid_name(fifo_name);
        let data = fifo_data_name(fifo_name);
        for (rule, is_enq, value) in touching {
            cx.enter_rule(rule);
            let f = &fires_name[&rule];
            let _ = writeln!(fifo_body, "    when {f} :");
            if is_enq {
                let value = cx
                    .compile_expr_hinted(value.expect("Enq[] always carries a value"), Some(*width))
                    .unwrap_or_default();
                let _ = writeln!(fifo_body, "      connect {valid}, UInt<1>(1)");
                let _ = writeln!(fifo_body, "      connect {data}, {value}");
            } else {
                let _ = writeln!(fifo_body, "      connect {valid}, UInt<1>(0)");
            }
        }
    }

    // Registers: same priority-mux pattern; unwritten paths hold by
    // FIRRTL's default register semantics, no explicit else needed. A
    // register written on only one side of an if/else (SUBLEQ's branch:
    // `if r <= 0 { pc := c } else { pc := pc + 3 }`) gets its value
    // threaded through as a `mux`, not silently dropped for not being a
    // top-level assignment.
    let mut reg_body = String::new();
    for (match_name, emit_name, width, _) in &regs {
        let mut values: Vec<(ItemId, String)> = Vec::new();
        for rule in &rules {
            cx.enter_rule(*rule);
            let body = rule_body(ast, *rule);
            if let Some(v) = cx.reg_value_in_stmts(&body, match_name, *width) {
                values.push((*rule, v));
            }
        }
        if values.is_empty() {
            continue;
        }
        values.sort_by_key(|(r, _)| std::cmp::Reverse(order.iter().position(|x| x == r)));
        for (rule, value) in values {
            let f = &fires_name[&rule];
            let _ = writeln!(reg_body, "    when {f} :");
            let _ = writeln!(reg_body, "      connect {emit_name}, {value}");
        }
    }

    // Instances: `clock`/`reset` are ordinary input ports on any FIRRTL
    // module, so they need driving just like any other instance input —
    // unconditionally, not gated by a rule (an instance's own body needs
    // them on every cycle, not just cycles where the parent happens to
    // touch one of its other ports). Every other input port defaults to
    // 0, then the (at most one) firing rule that writes it overrides via
    // last-connect, same priority-mux pattern as a mem writer. Output
    // ports need no wiring here: reading `inst.port` compiles straight to
    // the FIRRTL reference `inst.port` (see `compile_expr_hinted`).
    let mut instance_decls = String::new();
    let mut instance_body = String::new();
    for (inst_name, target_name, inst_def) in &instances {
        let _ = writeln!(instance_decls, "    inst {inst_name} of {target_name}");
        let _ = writeln!(instance_body, "    connect {inst_name}.clock, clock");
        let _ = writeln!(instance_body, "    connect {inst_name}.reset, reset");
        let ports = types
            .instance_module
            .get(inst_def)
            .and_then(|m| types.module_ports.get(m))
            .cloned()
            .unwrap_or_default();
        for (port_name, kind, ty) in &ports {
            if *kind == DefKind::Input {
                let w = port_bit_width(ty).unwrap_or(1);
                let _ = writeln!(
                    instance_body,
                    "    connect {inst_name}.{port_name}, UInt<{w}>(0)"
                );
            }
        }
        // Same priority-mux pattern as a register (see `reg_value_in_stmts`):
        // an if/else-nested port write threads through a `mux`, falling back
        // to the unconditional `UInt(0)` default above on any path that
        // doesn't write it.
        for (port_name, kind, ty) in &ports {
            if *kind != DefKind::Input {
                continue;
            }
            let w = port_bit_width(ty).unwrap_or(1);
            let mut values: Vec<(ItemId, String)> = Vec::new();
            for rule in &rules {
                cx.enter_rule(*rule);
                let body = rule_body(ast, *rule);
                if let Some(v) = cx.inst_port_value_in_stmts(&body, inst_name, port_name, w) {
                    values.push((*rule, v));
                }
            }
            if values.is_empty() {
                continue;
            }
            values.sort_by_key(|(r, _)| std::cmp::Reverse(order.iter().position(|x| x == r)));
            for (rule, value) in values {
                let f = &fires_name[&rule];
                let _ = writeln!(instance_body, "    when {f} :");
                let _ = writeln!(
                    instance_body,
                    "      connect {inst_name}.{port_name}, {value}"
                );
            }
        }
    }

    if !cx.errors.is_empty() {
        return Err(cx.errors);
    }

    let mut body = String::new();
    body.push_str(&fires_body);
    body.push('\n');
    body.push_str(&instance_body);
    body.push('\n');
    body.push_str(&mem_body);
    body.push('\n');
    body.push_str(&fifo_body);
    body.push('\n');
    body.push_str(&reg_body);
    body.push('\n');
    body.push_str(&port_connects);
    Ok(module_block(
        &mod_name.text,
        is_public,
        &regs,
        &mems,
        &mem_ports,
        &inputs,
        &outputs,
        &instance_decls,
        &body,
    ))
}

fn port_bit_width(ty: &Ty) -> Option<u64> {
    match ty {
        Ty::Bits(Width::Known(w)) => Some(*w),
        _ => None,
    }
}

fn module_block(
    name: &str,
    is_public: bool,
    regs: &[(String, String, u64, u64)],
    mems: &[(String, u64, u64)],
    mem_ports: &HashMap<String, (Vec<String>, Option<String>)>,
    inputs: &[(String, u64)],
    outputs: &[(String, String, u64)],
    instance_decls: &str,
    body: &str,
) -> String {
    let mut out = String::new();
    let kw = if is_public { "public module" } else { "module" };
    let _ = writeln!(out, "  {kw} {name} :");
    let _ = writeln!(out, "    input clock : Clock");
    let _ = writeln!(out, "    input reset : UInt<1>");
    for (n, w) in inputs {
        let _ = writeln!(out, "    input {n} : UInt<{w}>");
    }
    for (n, _, w) in outputs {
        let _ = writeln!(out, "    output {n} : UInt<{w}>");
    }
    out.push('\n');
    out.push_str(instance_decls);
    if !instance_decls.is_empty() {
        out.push('\n');
    }
    for (_, emit_name, w, init) in regs {
        let _ = writeln!(
            out,
            "    regreset {emit_name} : UInt<{w}>, clock, reset, UInt<{w}>({init})"
        );
    }
    out.push('\n');
    for (n, w, len) in mems {
        let default = (Vec::new(), None);
        let (readers, writer) = mem_ports.get(n).unwrap_or(&default);
        let _ = writeln!(out, "    mem {n} :");
        let _ = writeln!(out, "      data-type => UInt<{w}>");
        let _ = writeln!(out, "      depth => {len}");
        for r in readers {
            let _ = writeln!(out, "      reader => {r}");
        }
        if let Some(w) = writer {
            let _ = writeln!(out, "      writer => {w}");
        }
        let _ = writeln!(out, "      read-latency => 0");
        let _ = writeln!(out, "      write-latency => 1");
        let _ = writeln!(out, "      read-under-write => undefined");
    }
    out.push('\n');
    out.push_str(body);
    out
}

fn fifo_valid_name(fifo: &str) -> String {
    format!("__fifo_{fifo}_valid")
}

fn fifo_data_name(fifo: &str) -> String {
    format!("__fifo_{fifo}_data")
}

/// `Deq[]` succeeds iff the fifo is valid; `Enq[x]` succeeds iff it is
/// not (depth-1: there is no room for a second element).
fn fifo_guard_cond(fifo: &str, is_enq: bool) -> String {
    let valid = fifo_valid_name(fifo);
    if is_enq {
        format!("not({valid})")
    } else {
        valid
    }
}

fn is_fifo_op(ast: &Ast, res: &Resolution, expr: ExprId) -> bool {
    let Expr::Bracket { callee, .. } = ast.expr(expr) else {
        return false;
    };
    let Expr::Field { base, name } = ast.expr(*callee) else {
        return false;
    };
    matches!(name.as_str(), "Enq" | "Deq")
        && res
            .expr_defs
            .get(base)
            .is_some_and(|d| res.def(*d).kind == DefKind::Fifo)
}

fn contains_fifo_op(ast: &Ast, res: &Resolution, stmt: StmtId) -> bool {
    match ast.stmt(stmt) {
        Stmt::Expr(e) => is_fifo_op(ast, res, *e),
        Stmt::Assign { rhs, .. } => is_fifo_op(ast, res, *rhs),
        Stmt::If {
            then_body,
            else_body,
            ..
        } => {
            then_body.iter().any(|s| contains_fifo_op(ast, res, *s))
                || else_body
                    .as_ref()
                    .is_some_and(|b| b.iter().any(|s| contains_fifo_op(ast, res, *s)))
        }
        Stmt::While { body, .. } => body.iter().any(|s| contains_fifo_op(ast, res, *s)),
        _ => false,
    }
}

fn clog2(v: u64) -> u64 {
    if v <= 1 {
        0
    } else {
        64 - (v - 1).leading_zeros() as u64
    }
}

fn item_name(ast: &Ast, id: ItemId) -> &str {
    match ast.item(id) {
        Item::Rule { name, .. } => &name.text,
        _ => "?",
    }
}

fn rule_body(ast: &Ast, id: ItemId) -> Vec<StmtId> {
    match ast.item(id) {
        Item::Rule { body, .. } => body.clone(),
        _ => Vec::new(),
    }
}

/// Rules that write `mem_name[..] := ..` (write side of the aggregate
/// row already computed by effects.rs; re-derived here structurally
/// since emission needs the actual address/data expressions).
fn writers_of(ast: &Ast, res: &Resolution, rules: &[ItemId], mem_name: &str) -> Vec<ItemId> {
    rules
        .iter()
        .copied()
        .filter(|r| find_mem_write(ast, res, &rule_body(ast, *r), mem_name).is_some())
        .collect()
}

fn find_mem_write(ast: &Ast, res: &Resolution, stmts: &[StmtId], mem_name: &str) -> Option<StmtId> {
    stmts
        .iter()
        .copied()
        .find(|s| is_mem_write_to(ast, res, *s, mem_name))
}

/// True for a plain register write OR an output write (`sum := ...`) —
/// both are matched by the user's own name; an output's *emitted* target
/// is its internal backing register, resolved separately (see
/// `Emitter::output_regs`).
fn is_ident_named(ast: &Ast, res: &Resolution, id: ExprId, name: &str) -> bool {
    matches!(ast.expr(id), Expr::Ident(_))
        && res.expr_defs.get(&id).is_some_and(|d| {
            let d = res.def(*d);
            d.name == name && matches!(d.kind, DefKind::Reg | DefKind::Output)
        })
}

fn is_mem_write_to(ast: &Ast, res: &Resolution, stmt: StmtId, mem_name: &str) -> bool {
    let Stmt::Assign { lhs, .. } = ast.stmt(stmt) else {
        return false;
    };
    let Expr::Bracket { callee, .. } = ast.expr(*lhs) else {
        return false;
    };
    matches!(res.expr_defs.get(callee), Some(d) if res.def(*d).name == mem_name)
}

/// A memory write nested inside `if`/`while` is not supported: unlike
/// register writes (which thread through a `mux`), memory writes also
/// need an addr/data pair muxed together, and no example needs it yet.
/// Only descends into control flow — a top-level write is fine and is
/// found separately by `find_mem_write`.
fn find_nested_mem_write(ast: &Ast, stmts: &[StmtId]) -> Option<Span> {
    for stmt in stmts {
        match ast.stmt(*stmt) {
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                if let Some(s) = find_any_mem_write_deep(ast, then_body) {
                    return Some(ast.stmt_spans[s.0 as usize].clone());
                }
                if let Some(s) = else_body
                    .as_deref()
                    .and_then(|b| find_any_mem_write_deep(ast, b))
                {
                    return Some(ast.stmt_spans[s.0 as usize].clone());
                }
            }
            Stmt::While { body, .. } => {
                if let Some(s) = find_any_mem_write_deep(ast, body) {
                    return Some(ast.stmt_spans[s.0 as usize].clone());
                }
            }
            _ => {}
        }
    }
    None
}

fn find_any_mem_write_deep(ast: &Ast, stmts: &[StmtId]) -> Option<StmtId> {
    for stmt in stmts {
        // Bracket-indexed assignment is only produced by a mem write
        // (`m[addr] := v`) — no other lvalue in the grammar has this
        // shape — so a structural check is precise without `Resolution`.
        if let Stmt::Assign { lhs, .. } = ast.stmt(*stmt)
            && matches!(ast.expr(*lhs), Expr::Bracket { .. })
        {
            return Some(*stmt);
        }
        let nested = match ast.stmt(*stmt) {
            Stmt::If {
                then_body,
                else_body,
                ..
            } => find_any_mem_write_deep(ast, then_body).or_else(|| {
                else_body
                    .as_deref()
                    .and_then(|b| find_any_mem_write_deep(ast, b))
            }),
            Stmt::While { body, .. } => find_any_mem_write_deep(ast, body),
            _ => None,
        };
        if nested.is_some() {
            return nested;
        }
    }
    None
}

fn is_ident_named_inst(ast: &Ast, res: &Resolution, id: ExprId, name: &str) -> bool {
    matches!(ast.expr(id), Expr::Ident(_))
        && res.expr_defs.get(&id).is_some_and(|d| {
            let d = res.def(*d);
            d.name == name && d.kind == DefKind::Inst
        })
}

/// Whether any `Expr::Call` appears anywhere in `id`'s subtree — used to
/// reject a callee whose own body calls something else (see
/// `Emitter::compile_call`'s doc comment for why that rules out
/// recursion too, not just deep inlining).
fn expr_contains_call(ast: &Ast, id: ExprId) -> bool {
    match ast.expr(id) {
        Expr::Call { .. } => true,
        Expr::Ident(_) | Expr::Int(_) | Expr::Wildcard => false,
        Expr::Unary { operand, .. } => expr_contains_call(ast, *operand),
        Expr::Binary { lhs, rhs, .. } => {
            expr_contains_call(ast, *lhs) || expr_contains_call(ast, *rhs)
        }
        Expr::Guard(inner) | Expr::Spawn(inner) => expr_contains_call(ast, *inner),
        Expr::Field { base, .. } => expr_contains_call(ast, *base),
        Expr::Bracket { callee, args } => {
            expr_contains_call(ast, *callee) || args.iter().any(|a| expr_contains_call(ast, *a))
        }
    }
}

struct Emitter<'a> {
    ast: &'a Ast,
    res: &'a Resolution,
    fx: &'a Effects,
    types: &'a Types,
    errors: Vec<EmitError>,
    /// Each static mem-read expression -> its assigned reader port name.
    read_ports: HashMap<ExprId, String>,
    /// Output port name -> its internal backing register name. Reading
    /// an output inside a rule (`sum := sum + inc`) must see the
    /// register, not the port (a FIRRTL output port is drive-only from
    /// inside its own module in the shape this emitter produces).
    output_regs: HashMap<String, String>,
    /// The rule currently being compiled: each local's binding
    /// expression. Locals have no FIRRTL declaration of their own —
    /// they are wires — so a reference to one inlines (recursively
    /// compiles) its binding instead of emitting an undeclared
    /// identifier. Refreshed by `enter_rule` before compiling any part
    /// of a rule; only that rule's top-level bindings are visible,
    /// matching source scoping (a local from inside `if`/`while` cannot
    /// be referenced outside it).
    locals: HashMap<DefId, ExprId>,
}

impl<'a> Emitter<'a> {
    fn error(&mut self, span: Span, message: String) {
        self.errors.push(EmitError { span, message });
    }

    fn state_width(&self, def: DefId) -> Option<Ty> {
        self.types.state_tys.get(&def).cloned()
    }

    fn state_mem_ty(&self, def: DefId) -> Option<Ty> {
        self.types.state_tys.get(&def).cloned()
    }

    fn const_eval(&self, id: ExprId) -> Option<u64> {
        match self.ast.expr(id) {
            Expr::Int(v) => Some(*v),
            _ => None,
        }
    }

    fn collect_read_sites(&mut self, stmts: &[StmtId]) {
        for stmt in stmts {
            match self.ast.stmt(*stmt).clone() {
                Stmt::Assign { lhs, rhs } => {
                    self.collect_read_sites_expr(rhs);
                    if let Expr::Bracket { args, .. } = self.ast.expr(lhs).clone() {
                        for a in args {
                            self.collect_read_sites_expr(a);
                        }
                    }
                }
                Stmt::Expr(e) => self.collect_read_sites_expr(e),
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    self.collect_read_sites_expr(cond);
                    self.collect_read_sites(&then_body);
                    if let Some(e) = else_body {
                        self.collect_read_sites(&e);
                    }
                }
                _ => {}
            }
        }
    }

    fn collect_read_sites_expr(&mut self, id: ExprId) {
        if let Expr::Bracket { callee, .. } = self.ast.expr(id).clone()
            && let Expr::Ident(_) = self.ast.expr(callee)
            && let Some(def) = self.res.expr_defs.get(&callee)
            && self.res.def(*def).kind == DefKind::Mem
        {
            let n = self.read_ports.len();
            self.read_ports.insert(id, format!("r{n}"));
            return; // the address sub-expr is compiled, not walked further
        }
        for child in crate::lower::sub_exprs(self.ast, id) {
            self.collect_read_sites_expr(child);
        }
    }

    /// `expr` is `<fifo>.Enq[x]` or `<fifo>.Deq[]` -> the fifo's name,
    /// whether it is `Enq`, and (for `Enq`) the value argument.
    fn fifo_op(&self, expr: ExprId) -> Option<(String, bool, Option<ExprId>)> {
        let Expr::Bracket { callee, args } = self.ast.expr(expr) else {
            return None;
        };
        let Expr::Field { base, name } = self.ast.expr(*callee) else {
            return None;
        };
        let def = self.res.expr_defs.get(base)?;
        if self.res.def(*def).kind != DefKind::Fifo {
            return None;
        }
        let fifo = self.res.def(*def).name.clone();
        match name.as_str() {
            "Deq" => Some((fifo, false, None)),
            "Enq" => Some((fifo, true, args.first().copied())),
            _ => None,
        }
    }

    /// The fifo op directly reachable from `stmt`, if any — the two
    /// shapes DESIGN.md's examples use: `x := f.Deq[]` and a bare
    /// `f.Enq[x]` statement.
    fn fifo_op_stmt(&self, stmt: StmtId) -> Option<(String, bool, Option<ExprId>)> {
        let expr = match self.ast.stmt(stmt) {
            Stmt::Expr(e) => *e,
            Stmt::Assign { rhs, .. } => *rhs,
            _ => return None,
        };
        self.fifo_op(expr)
    }

    /// A guard (`expr?`) or fifo op (`Enq[x]`/`Deq[]`) may only appear
    /// before any state write in the same rule, and only at the top
    /// level — both are failure conditions that must gate the whole
    /// rule, per the module doc comment.
    fn check_guard_placement(&mut self, rule: ItemId) {
        let body = rule_body(self.ast, rule);
        let mut seen_write = false;
        for stmt in &body {
            match self.ast.stmt(*stmt).clone() {
                Stmt::Assign { lhs, rhs } => {
                    if is_state_write(self.ast, self.res, lhs) {
                        seen_write = true;
                    } else if self.fifo_op(rhs).is_some() && seen_write {
                        self.error(
                            self.ast.stmt_spans[stmt.0 as usize].clone(),
                            "a fifo operation after a state write is not yet supported \
                             (v0 restriction): it must gate the whole rule"
                                .to_string(),
                        );
                    }
                }
                Stmt::Expr(e) => {
                    if matches!(self.ast.expr(e), Expr::Guard(_)) && seen_write {
                        self.error(
                            self.ast.expr_spans[e.0 as usize].clone(),
                            "a guard after a state write is not yet supported (v0 \
                             restriction): a guard must gate the whole rule"
                                .to_string(),
                        );
                    } else if self.fifo_op(e).is_some() && seen_write {
                        self.error(
                            self.ast.expr_spans[e.0 as usize].clone(),
                            "a fifo operation after a state write is not yet supported \
                             (v0 restriction): it must gate the whole rule"
                                .to_string(),
                        );
                    }
                }
                Stmt::If { .. } | Stmt::While { .. } => {
                    if contains_guard(self.ast, *stmt) {
                        self.error(
                            self.ast.stmt_spans[stmt.0 as usize].clone(),
                            "a guard nested in if/while is not yet supported (v0 restriction)"
                                .to_string(),
                        );
                    }
                    if contains_fifo_op(self.ast, self.res, *stmt) {
                        self.error(
                            self.ast.stmt_spans[stmt.0 as usize].clone(),
                            "a fifo operation nested in if/while is not yet supported \
                             (v0 restriction)"
                                .to_string(),
                        );
                    }
                }
                _ => {}
            }
        }
    }

    /// A local reassigned within one emitted rule is not supported: a
    /// later reference to it would need to know *which* assignment it
    /// follows (locals are inlined by binding, not by program order —
    /// see `enter_rule`), and picking the wrong one silently compiles a
    /// different value than the source reads. Reject it outright rather
    /// than risk that.
    fn check_no_reassigned_locals(&mut self, rule: ItemId) {
        let body = rule_body(self.ast, rule);
        let mut seen: std::collections::HashSet<DefId> = Default::default();
        for stmt in &body {
            let Stmt::Assign { lhs, .. } = self.ast.stmt(*stmt) else {
                continue;
            };
            let Some(def) = self.res.expr_defs.get(lhs).copied() else {
                continue;
            };
            if self.res.def(def).kind != DefKind::Local {
                continue;
            }
            if !seen.insert(def) {
                self.error(
                    self.ast.stmt_spans[stmt.0 as usize].clone(),
                    format!(
                        "`{}` is reassigned in this rule; FIRRTL emission does not yet \
                         support reassigning a local (v0 restriction: locals are \
                         inlined at their single binding site, not read in program \
                         order)",
                        self.res.def(def).name
                    ),
                );
            }
        }
    }

    /// A depth-1 fifo cannot both `Enq` and `Deq` in the same cycle:
    /// that would require its valid bit to be both 1 (for `Deq`) and 0
    /// (for `Enq`) at once, an always-false guard. Reject it explicitly
    /// rather than silently synthesizing permanently dead hardware.
    fn check_fifo_same_cycle(&mut self, rule: ItemId) {
        let body = rule_body(self.ast, rule);
        let mut enqueued = std::collections::HashSet::new();
        let mut dequeued = std::collections::HashSet::new();
        for stmt in &body {
            let Some((fifo, is_enq, _)) = self.fifo_op_stmt(*stmt) else {
                continue;
            };
            if is_enq {
                enqueued.insert(fifo);
            } else {
                dequeued.insert(fifo);
            }
        }
        for fifo in enqueued.intersection(&dequeued) {
            self.error(
                self.ast.item_spans[rule.0 as usize].clone(),
                format!(
                    "this rule both enqueues and dequeues `{fifo}` in the same cycle; \
                     not supported for a depth-1 fifo (v0 restriction): split into two \
                     rules"
                ),
            );
        }
    }

    /// This rule's local bindings, refreshed before compiling any part
    /// of it. See the `locals` field doc comment.
    fn enter_rule(&mut self, rule: ItemId) {
        self.locals.clear();
        let body = rule_body(self.ast, rule);
        for stmt in &body {
            match self.ast.stmt(*stmt).clone() {
                Stmt::Assign { lhs, rhs } => {
                    if let Some(def) = self.res.expr_defs.get(&lhs).copied()
                        && self.res.def(def).kind == DefKind::Local
                    {
                        self.locals.insert(def, rhs);
                    }
                }
                Stmt::Let { name, init } => {
                    if let Some((i, _)) = self
                        .res
                        .defs
                        .iter()
                        .enumerate()
                        .find(|(_, d)| d.span == name.span)
                    {
                        self.locals.insert(DefId(i as u32), init);
                    }
                }
                _ => {}
            }
        }
    }

    fn compile_guard(&mut self, rule: ItemId) -> String {
        let body = rule_body(self.ast, rule);
        let mut conds = Vec::new();
        for stmt in &body {
            match self.ast.stmt(*stmt).clone() {
                Stmt::Expr(e) => {
                    if let Expr::Guard(inner) = self.ast.expr(e) {
                        conds.push(
                            self.compile_expr(*inner)
                                .unwrap_or_else(|_| "UInt<1>(1)".to_string()),
                        );
                    } else if let Some((fifo, is_enq, _)) = self.fifo_op(e) {
                        conds.push(fifo_guard_cond(&fifo, is_enq));
                    }
                }
                Stmt::Assign { rhs, .. } => {
                    if let Some((fifo, is_enq, _)) = self.fifo_op(rhs) {
                        conds.push(fifo_guard_cond(&fifo, is_enq));
                    }
                }
                _ => {}
            }
        }
        conds
            .into_iter()
            .reduce(|a, b| format!("and({a}, {b})"))
            .unwrap_or_else(|| "UInt<1>(1)".to_string())
    }

    fn write_target(&mut self, rule: ItemId, mem_name: &str, elem_width: u64) -> (String, String) {
        let body = rule_body(self.ast, rule);
        let Some(stmt) = find_mem_write(self.ast, self.res, &body, mem_name) else {
            return ("UInt<1>(0)".to_string(), "UInt<1>(0)".to_string());
        };
        let Stmt::Assign { lhs, rhs } = self.ast.stmt(stmt).clone() else {
            unreachable!()
        };
        let Expr::Bracket { args, .. } = self.ast.expr(lhs).clone() else {
            unreachable!()
        };
        let addr = self.compile_expr(args[0]).unwrap_or_default();
        let data = self
            .compile_expr_hinted(rhs, Some(elem_width))
            .unwrap_or_default();
        (addr, data)
    }

    /// The value `reg_name` takes on when this rule's statements run,
    /// threading assignments through nested if/else as a `mux` tree.
    /// `None` means this rule never assigns the register at all. A
    /// branch that doesn't assign it falls back to whatever value was
    /// already accumulated (an earlier assignment in the same rule) or,
    /// failing that, the register's own current value — i.e. it holds,
    /// exactly like an ordinary un-driven path would.
    fn reg_value_in_stmts(
        &mut self,
        stmts: &[StmtId],
        reg_name: &str,
        width: u64,
    ) -> Option<String> {
        let mut current: Option<String> = None;
        for stmt in stmts {
            match self.ast.stmt(*stmt).clone() {
                Stmt::Assign { lhs, rhs } if is_ident_named(self.ast, self.res, lhs, reg_name) => {
                    current = Some(
                        self.compile_expr_hinted(rhs, Some(width))
                            .unwrap_or_default(),
                    );
                }
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    let then_val = self.reg_value_in_stmts(&then_body, reg_name, width);
                    let else_val = else_body
                        .as_ref()
                        .and_then(|b| self.reg_value_in_stmts(b, reg_name, width));
                    if then_val.is_some() || else_val.is_some() {
                        let hold = current.clone().unwrap_or_else(|| reg_name.to_string());
                        let t = then_val.unwrap_or_else(|| hold.clone());
                        let e = else_val.unwrap_or(hold);
                        let cond_str = self
                            .compile_expr(cond)
                            .unwrap_or_else(|_| "UInt<1>(0)".to_string());
                        current = Some(format!("mux({cond_str}, {t}, {e})"));
                    }
                }
                Stmt::While { .. } => {
                    self.error(
                        self.ast.stmt_spans[stmt.0 as usize].clone(),
                        "a loop in an emitted rule body is not supported (sequences \
                         lowering should have removed it before emission)"
                            .to_string(),
                    );
                }
                _ => {}
            }
        }
        current
    }

    /// Same threading as `reg_value_in_stmts`, for an instance's input
    /// port: an if/else-nested write folds into a `mux`. A register's
    /// unwritten path holds its own feedback; a port has no state of its
    /// own, so its unwritten path falls back to the literal `UInt(0)`
    /// already connected unconditionally before any rule's `when` block.
    fn inst_port_value_in_stmts(
        &mut self,
        stmts: &[StmtId],
        inst_name: &str,
        port_name: &str,
        width: u64,
    ) -> Option<String> {
        let mut current: Option<String> = None;
        for stmt in stmts {
            match self.ast.stmt(*stmt).clone() {
                Stmt::Assign { lhs, rhs } => {
                    if let Expr::Field { base, name } = self.ast.expr(lhs).clone()
                        && name == port_name
                        && is_ident_named_inst(self.ast, self.res, base, inst_name)
                    {
                        current = Some(
                            self.compile_expr_hinted(rhs, Some(width))
                                .unwrap_or_default(),
                        );
                    }
                }
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    let then_val =
                        self.inst_port_value_in_stmts(&then_body, inst_name, port_name, width);
                    let else_val = else_body.as_ref().and_then(|b| {
                        self.inst_port_value_in_stmts(b, inst_name, port_name, width)
                    });
                    if then_val.is_some() || else_val.is_some() {
                        let hold = current
                            .clone()
                            .unwrap_or_else(|| format!("UInt<{width}>(0)"));
                        let t = then_val.unwrap_or_else(|| hold.clone());
                        let e = else_val.unwrap_or(hold);
                        let cond_str = self
                            .compile_expr(cond)
                            .unwrap_or_else(|_| "UInt<1>(0)".to_string());
                        current = Some(format!("mux({cond_str}, {t}, {e})"));
                    }
                }
                Stmt::While { .. } => {
                    self.error(
                        self.ast.stmt_spans[stmt.0 as usize].clone(),
                        "a loop in an emitted rule body is not supported (sequences \
                         lowering should have removed it before emission)"
                            .to_string(),
                    );
                }
                _ => {}
            }
        }
        current
    }

    fn width_of(&mut self, id: ExprId) -> u64 {
        match self.types.expr_tys.get(&id) {
            Some(Ty::Bits(Width::Known(w))) => *w,
            _ => {
                self.error(
                    self.ast.expr_spans[id.0 as usize].clone(),
                    "no concrete width for this expression; FIRRTL emission needs one".to_string(),
                );
                1
            }
        }
    }

    /// Top-level entry: no width hint, so a bare literal falls back to
    /// its own (usually absent) type. Prefer `compile_expr_hinted` from
    /// any caller that knows the width the literal should take on —
    /// which is everywhere a literal can legally appear, since our type
    /// checker never gives a literal its own concrete width (it only
    /// *absorbs* one from its context).
    fn compile_expr(&mut self, id: ExprId) -> Result<String, ()> {
        self.compile_expr_hinted(id, None)
    }

    fn compile_expr_hinted(&mut self, id: ExprId, hint: Option<u64>) -> Result<String, ()> {
        if let Some(port) = self.read_ports.get(&id) {
            let Expr::Bracket { callee, .. } = self.ast.expr(id) else {
                unreachable!()
            };
            let Expr::Ident(mem_name) = self.ast.expr(*callee) else {
                unreachable!()
            };
            return Ok(format!("{mem_name}.{port}.data"));
        }
        if let Some((fifo, is_enq, _)) = self.fifo_op(id)
            && !is_enq
        {
            return Ok(fifo_data_name(&fifo));
        }
        // `inst.port` reading a child's output port: a plain combinational
        // reference, always valid (the child drives it unconditionally),
        // no gating needed — direction is already checked by types.rs.
        if let Expr::Field { base, name } = self.ast.expr(id).clone()
            && let Some(def) = self.res.expr_defs.get(&base)
            && self.res.def(*def).kind == DefKind::Inst
        {
            let inst_name = self.res.def(*def).name.clone();
            return Ok(format!("{inst_name}.{name}"));
        }
        match self.ast.expr(id).clone() {
            Expr::Ident(_) => {
                let def = self.res.expr_defs.get(&id).copied();
                match def.map(|d| self.res.def(d).clone()) {
                    Some(d) if d.kind == DefKind::Output => Ok(self.output_regs[&d.name].clone()),
                    Some(d) if matches!(d.kind, DefKind::Local | DefKind::Param) => {
                        match self.locals.get(&def.unwrap()) {
                            Some(bound) => self.compile_expr_hinted(*bound, hint),
                            None => {
                                self.error(
                                    self.ast.expr_spans[id.0 as usize].clone(),
                                    "cannot find this local's binding in the rule \
                                     currently being compiled (v0 restriction: a local \
                                     or a called function's parameter is only resolved \
                                     within its own rule/call)"
                                        .to_string(),
                                );
                                Err(())
                            }
                        }
                    }
                    Some(d) if matches!(d.kind, DefKind::Reg | DefKind::Input) => Ok(d.name),
                    _ => {
                        self.error(
                            self.ast.expr_spans[id.0 as usize].clone(),
                            "unsupported reference in FIRRTL emission (v0 restriction)".to_string(),
                        );
                        Err(())
                    }
                }
            }
            Expr::Int(v) => {
                let w = hint.unwrap_or_else(|| self.width_of(id));
                Ok(format!("UInt<{w}>({v})"))
            }
            Expr::Binary { op, lhs, rhs } => self.compile_binop(id, op, lhs, rhs),
            Expr::Unary { op, operand } => self.compile_unop(id, op, operand),
            Expr::Bracket { callee, args } => {
                if matches!(self.types.expr_tys.get(&callee), Some(Ty::Bits(_))) {
                    self.compile_bit_select(callee, &args)
                } else {
                    self.error(
                        self.ast.expr_spans[id.0 as usize].clone(),
                        "this indexing form is not yet supported in FIRRTL emission (v0 \
                         restriction: only memory reads and bit-select/slice on a plain \
                         identifier)"
                            .to_string(),
                    );
                    Err(())
                }
            }
            Expr::Call { callee, args } => {
                match self
                    .res
                    .expr_defs
                    .get(&callee)
                    .map(|d| self.res.def(*d).kind)
                {
                    Some(DefKind::Fn | DefKind::Impl) => self.compile_call(id, callee, &args, hint),
                    _ => {
                        self.error(
                            self.ast.expr_spans[id.0 as usize].clone(),
                            "this call is not yet supported in FIRRTL emission (v0 \
                             restriction: only a call to a user `fn`/`impl` with a \
                             simple body — `let` bindings, `if`/`else` branches, and a \
                             trailing `return`, no state writes, no nested calls — can \
                             be inlined; builtin calls like `prio` are not yet \
                             synthesizable)"
                                .to_string(),
                        );
                        Err(())
                    }
                }
            }
            _ => {
                self.error(
                    self.ast.expr_spans[id.0 as usize].clone(),
                    "this expression form is not yet supported in FIRRTL emission (v0 \
                     restriction: identifiers, integers, arithmetic/bitwise/shift \
                     operators, comparisons, unary -/~, bit-select/slice, and memory \
                     reads only)"
                        .to_string(),
                );
                Err(())
            }
        }
    }

    /// `x[i]` or `x[hi..lo]` on a bits-typed (not memory) base. FIRRTL's
    /// `bits` primop needs static bounds, so both forms require literal
    /// integer indices — a computed bound is a v0 restriction, not a
    /// missing feature the type checker would otherwise reject (it
    /// happily types a dynamic single-bit select as `bits[1]`).
    fn compile_bit_select(&mut self, callee: ExprId, args: &[ExprId]) -> Result<String, ()> {
        let base = self.compile_expr(callee)?;
        let Some(&arg) = args.first() else {
            self.error(
                self.ast.expr_spans[callee.0 as usize].clone(),
                "bit-select/slice takes exactly one argument".to_string(),
            );
            return Err(());
        };
        let bounds = if let Expr::Binary {
            op: BinOp::Range,
            lhs,
            rhs,
        } = self.ast.expr(arg)
        {
            match (self.ast.expr(*lhs), self.ast.expr(*rhs)) {
                (Expr::Int(hi), Expr::Int(lo)) => Some((*hi, *lo)),
                _ => None,
            }
        } else if let Expr::Int(i) = self.ast.expr(arg) {
            Some((*i, *i))
        } else {
            None
        };
        let Some((hi, lo)) = bounds else {
            self.error(
                self.ast.expr_spans[arg.0 as usize].clone(),
                "bit-select/slice bounds must be literal integers in FIRRTL emission \
                 (v0 restriction: no computed bit-select bounds)"
                    .to_string(),
            );
            return Err(());
        };
        if hi < lo {
            self.error(
                self.ast.expr_spans[arg.0 as usize].clone(),
                format!(
                    "slice bounds must be high..low (got {hi}..{lo}); the type checker \
                     accepts either order but FIRRTL's `bits` primop needs hi >= lo"
                ),
            );
            return Err(());
        }
        Ok(format!("bits({base}, {hi}, {lo})"))
    }

    /// Inlines a call to a user `fn`/`impl`: FIRRTL has no call concept,
    /// so the callee's body is spliced into the caller at its call site
    /// rather than emitted as its own hardware. v0 restricts the callee
    /// to a pure value computation the width-hint machinery can thread
    /// through unchanged — anything else is an explicit error, not a
    /// silent miscompile:
    /// - no `<sequences>`/`<elaborates>` color, no state writes, and no
    ///   possible failure (a guard inside the body would already set
    ///   this) — effects.rs's already-merged signature answers all three
    ///   in one check, including through the callee's own calls;
    /// - body shape: zero or more `let` bindings, then either a trailing
    ///   `return <expr>` or an `if`/`else` whose branches both recurse
    ///   into this same shape (mandatory `else` — every reachable path
    ///   must produce a value, folded into a `mux` by
    ///   `compile_callee_body`), no state writes (a redundant but
    ///   cheaper check than the signature one above), no further calls
    ///   anywhere in the tree (sidesteps recursion entirely: a function
    ///   whose own body cannot call anything can never call itself,
    ///   directly or through a cycle).
    ///
    /// Width correctness for a generic callee (`bits[N]` params): this
    /// call expression's own OUTER width (`id`, already instantiated to
    /// a concrete number by types.rs's call-site solver) is used as the
    /// hint threaded into every branch's return expression — never the
    /// callee's own internal, still-generic `types.expr_tys` entry for
    /// its return expression(s), which were only ever checked once, that
    /// generically, independent of any particular call site.
    fn compile_call(
        &mut self,
        id: ExprId,
        callee: ExprId,
        args: &[ExprId],
        hint: Option<u64>,
    ) -> Result<String, ()> {
        let span = self.ast.expr_spans[id.0 as usize].clone();
        let def = *self.res.expr_defs.get(&callee).expect("checked by caller");
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
        let Item::Fn {
            params,
            body: fn_body,
            ..
        } = self.ast.item(fn_item).clone()
        else {
            self.error(span, "call target is not a function".to_string());
            return Err(());
        };

        let Some(sig) = self.fx.sigs.get(&fn_item) else {
            self.error(
                span,
                "no effect signature computed for this function".to_string(),
            );
            return Err(());
        };
        if sig.sequences || sig.elaborates {
            self.error(
                span,
                "calling a <sequences>/<elaborates> function is not yet supported in \
                 FIRRTL emission (v0 restriction: only a pure <combines> value \
                 computation can be inlined)"
                    .to_string(),
            );
            return Err(());
        }
        if sig.fails {
            self.error(
                span,
                "calling a function that can fail (a guard in its body) is not yet \
                 supported in FIRRTL emission (v0 restriction: an inlined call cannot \
                 gate the caller's rule)"
                    .to_string(),
            );
            return Err(());
        }
        if !sig.writes.is_empty() {
            self.error(
                span,
                "calling a function that writes state is not yet supported in FIRRTL \
                 emission (v0 restriction: only a pure value-computing function can be \
                 inlined)"
                    .to_string(),
            );
            return Err(());
        }

        // Bind params into `self.locals`, saving whatever was there before
        // (from an enclosing call to this SAME function, if any) so it can
        // be restored once this call is fully compiled. Without this,
        // `Avg(Avg(x, y), z)` would silently miscompile: `Avg`'s param
        // DefIds are shared across every call to `Avg`, so compiling the
        // outer call's first argument (which recurses into the inner
        // `Avg(x, y)` call) would rebind them out from under the outer
        // call before it gets to compile its second argument — a real,
        // observed silent drop of `z`, not a hypothetical. Save/restore
        // makes this properly reentrant regardless of how deep or
        // indirect the nesting is (as an argument, or via a `let` whose
        // value is a call), not just the syntactically-nested case.
        let mut saved: Vec<(DefId, Option<ExprId>)> = Vec::new();
        for (param, arg) in params.iter().zip(args.iter()) {
            if let Some((i, _)) = self
                .res
                .defs
                .iter()
                .enumerate()
                .find(|(_, d)| d.span == param.name.span)
            {
                let def = DefId(i as u32);
                saved.push((def, self.locals.insert(def, *arg)));
            }
        }

        let w = hint.unwrap_or_else(|| self.width_of(id));
        let result = self.compile_callee_body(&fn_body, w, &span);

        for (def, prev) in saved.into_iter().rev() {
            match prev {
                Some(v) => {
                    self.locals.insert(def, v);
                }
                None => {
                    self.locals.remove(&def);
                }
            }
        }
        result
    }

    /// Compiles a callee's body (or an `if`/`else` branch of one, which
    /// has the identical shape) to a single value: zero or more `let`
    /// bindings, then either a trailing `return <expr>` or an `if`/`else`
    /// whose branches both recurse into this same shape. An `if` with no
    /// `else` is rejected — every reachable path must produce a value,
    /// there's no such thing as a "held" return the way an unwritten
    /// register path holds its own feedback. Each branch's `let`s are
    /// bound/restored around that branch's own recursive call, so they
    /// never leak into a sibling branch or the caller.
    fn compile_callee_body(
        &mut self,
        stmts: &[StmtId],
        hint: u64,
        span: &Span,
    ) -> Result<String, ()> {
        let Some((&last, lets)) = stmts.split_last() else {
            self.error(
                span.clone(),
                "calling a function with an empty body (or an empty `if`/`else` \
                 branch) is not yet supported in FIRRTL emission (v0 restriction: \
                 every branch must end with `return`)"
                    .to_string(),
            );
            return Err(());
        };
        if !lets
            .iter()
            .all(|s| matches!(self.ast.stmt(*s), Stmt::Let { .. }))
        {
            self.error(
                span.clone(),
                "this function's body is too complex to inline (v0 restriction: only \
                 `let` bindings, `if`/`else` branches, and a trailing `return` are \
                 supported — no state writes, loops, or fifo/guard operations)"
                    .to_string(),
            );
            return Err(());
        }
        if lets.iter().any(|s| {
            let Stmt::Let { init, .. } = self.ast.stmt(*s) else {
                unreachable!()
            };
            expr_contains_call(self.ast, *init)
        }) {
            self.error(
                span.clone(),
                "this function's body calls another function or builtin, which is not \
                 yet supported for inlining (v0 restriction: a called function's own \
                 body must not itself call anything, which also rules out recursion)"
                    .to_string(),
            );
            return Err(());
        }

        let mut saved: Vec<(DefId, Option<ExprId>)> = Vec::new();
        for s in lets {
            let Stmt::Let { name, init } = self.ast.stmt(*s) else {
                unreachable!()
            };
            if let Some((i, _)) = self
                .res
                .defs
                .iter()
                .enumerate()
                .find(|(_, d)| d.span == name.span)
            {
                let def = DefId(i as u32);
                saved.push((def, self.locals.insert(def, *init)));
            }
        }

        let result = match self.ast.stmt(last).clone() {
            Stmt::Return(Some(ret_expr)) => {
                if expr_contains_call(self.ast, ret_expr) {
                    self.error(
                        span.clone(),
                        "this function's body calls another function or builtin, \
                         which is not yet supported for inlining (v0 restriction: a \
                         called function's own body must not itself call anything, \
                         which also rules out recursion)"
                            .to_string(),
                    );
                    Err(())
                } else {
                    self.compile_expr_hinted(ret_expr, Some(hint))
                }
            }
            Stmt::If {
                cond,
                then_body,
                else_body: Some(else_body),
            } => {
                if expr_contains_call(self.ast, cond) {
                    self.error(
                        span.clone(),
                        "this function's body calls another function or builtin, \
                         which is not yet supported for inlining (v0 restriction: a \
                         called function's own body must not itself call anything, \
                         which also rules out recursion)"
                            .to_string(),
                    );
                    Err(())
                } else {
                    match (
                        self.compile_callee_body(&then_body, hint, span),
                        self.compile_callee_body(&else_body, hint, span),
                    ) {
                        (Ok(t), Ok(e)) => {
                            let cond_str = self
                                .compile_expr(cond)
                                .unwrap_or_else(|_| "UInt<1>(0)".to_string());
                            Ok(format!("mux({cond_str}, {t}, {e})"))
                        }
                        _ => Err(()),
                    }
                }
            }
            Stmt::If {
                else_body: None, ..
            } => {
                self.error(
                    span.clone(),
                    "an `if` inside an inlined function's body must have an `else` \
                     (v0 restriction: every reachable path must produce a value, \
                     there is no way to \"hold\" a return the way an unwritten \
                     register path holds its own feedback)"
                        .to_string(),
                );
                Err(())
            }
            _ => {
                self.error(
                    span.clone(),
                    "this function's body must end with `return <expr>`, or an \
                     `if`/`else` whose branches both do (v0 restriction)"
                        .to_string(),
                );
                Err(())
            }
        };

        for (def, prev) in saved.into_iter().rev() {
            match prev {
                Some(v) => {
                    self.locals.insert(def, v);
                }
                None => {
                    self.locals.remove(&def);
                }
            }
        }
        result
    }

    fn compile_unop(&mut self, id: ExprId, op: UnOp, operand: ExprId) -> Result<String, ()> {
        match op {
            UnOp::Neg => {
                let w = self.width_of(id);
                let e = self.compile_expr_hinted(operand, Some(w))?;
                Ok(format!("tail(sub(UInt<{w}>(0), {e}), 1)"))
            }
            UnOp::BitNot => {
                let w = self.width_of(id);
                let e = self.compile_expr_hinted(operand, Some(w))?;
                Ok(format!("not({e})"))
            }
            UnOp::Not => {
                self.error(
                    self.ast.expr_spans[id.0 as usize].clone(),
                    "logical `!` is not yet supported in FIRRTL emission (v0 \
                     restriction: use `~` for bitwise complement, or a comparison)"
                        .to_string(),
                );
                Err(())
            }
        }
    }

    /// A literal operand has no width of its own; it absorbs one from
    /// its sibling. Arithmetic already has the absorbed width recorded
    /// on the whole expression (`types.expr_tys[id]`); comparisons
    /// don't (their own type is always `bits[1]`), so fall back to
    /// whichever side is a concrete, non-literal type.
    fn compile_binop(
        &mut self,
        id: ExprId,
        op: BinOp,
        lhs: ExprId,
        rhs: ExprId,
    ) -> Result<String, ()> {
        if matches!(op, BinOp::Shl | BinOp::Shr) {
            return self.compile_shift(op, lhs, rhs);
        }
        let known_width = |types: &Types, e: ExprId| {
            types.expr_tys.get(&e).and_then(|t| match t {
                Ty::Bits(Width::Known(w)) => Some(*w),
                _ => None,
            })
        };
        // For every op below, one side being a bare literal (`Ty::Int`)
        // means the checker typed the whole expression as the *other*
        // side's own width (types.rs's mixed-operand rule), so hinting
        // the literal to `known_width(id)` always lands on the right
        // value — whether or not this op is one whose "both sides bits"
        // rule also happens to equal that width (it does for every op
        // here except Mul, handled below).
        let hint = if matches!(
            op,
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
        ) {
            known_width(self.types, id)
        } else if matches!(self.ast.expr(lhs), Expr::Int(_)) {
            known_width(self.types, rhs)
        } else if matches!(self.ast.expr(rhs), Expr::Int(_)) {
            known_width(self.types, lhs)
        } else {
            None
        };
        let l = self.compile_expr_hinted(lhs, hint)?;
        let r = self.compile_expr_hinted(rhs, hint)?;
        Ok(match op {
            BinOp::Add => format!("tail(add({l}, {r}), 1)"),
            BinOp::Sub => format!("tail(sub({l}, {r}), 1)"),
            BinOp::Mul => {
                // `mul` sums both compiled operand widths. When neither
                // side is a literal that already equals the checker's
                // target width (types.rs sums too, for a genuine
                // bits*bits multiply); a literal absorbed `hint` above
                // instead, so `mul` overshoots by exactly that width —
                // trim back down like `add`/`sub` do for their carry bit.
                let wl = if matches!(self.ast.expr(lhs), Expr::Int(_)) {
                    hint
                } else {
                    known_width(self.types, lhs)
                }
                .unwrap_or(1);
                let wr = if matches!(self.ast.expr(rhs), Expr::Int(_)) {
                    hint
                } else {
                    known_width(self.types, rhs)
                }
                .unwrap_or(1);
                let target = known_width(self.types, id).unwrap_or(wl + wr);
                match (wl + wr).checked_sub(target) {
                    Some(drop) if drop > 0 => format!("tail(mul({l}, {r}), {drop})"),
                    _ => format!("mul({l}, {r})"),
                }
            }
            BinOp::BitAnd => format!("and({l}, {r})"),
            BinOp::BitOr => format!("or({l}, {r})"),
            BinOp::BitXor => format!("xor({l}, {r})"),
            BinOp::Eq => format!("eq({l}, {r})"),
            BinOp::Ne => format!("neq({l}, {r})"),
            BinOp::Lt => format!("lt({l}, {r})"),
            BinOp::Le => format!("leq({l}, {r})"),
            BinOp::Gt => format!("gt({l}, {r})"),
            BinOp::Ge => format!("geq({l}, {r})"),
            _ => {
                self.error(
                    self.ast.expr_spans[id.0 as usize].clone(),
                    "this operator is not yet supported in FIRRTL emission (v0 \
                     restriction: div and rem are not supported)"
                        .to_string(),
                );
                return Err(());
            }
        })
    }

    /// Static (literal-amount) shifts only — FIRRTL's `shl`/`shr` need a
    /// constant, and a dynamic-amount `dshl`/`dshr` isn't wired up yet
    /// (v0 restriction). `shl` grows the width by the shift amount and
    /// `shr` shrinks it, but types.rs keeps the left operand's width for
    /// both, matching Verilog's fixed-width `<<`/`>>` — so both are
    /// brought back to that width: `shl` by dropping the high bits that
    /// fell off, `shr` by zero-padding back up.
    fn compile_shift(&mut self, op: BinOp, lhs: ExprId, rhs: ExprId) -> Result<String, ()> {
        let Expr::Int(n) = self.ast.expr(rhs) else {
            self.error(
                self.ast.expr_spans[rhs.0 as usize].clone(),
                "shift amount must be a literal integer in FIRRTL emission (v0 \
                 restriction: no variable-amount shifts)"
                    .to_string(),
            );
            return Err(());
        };
        let n = *n;
        let w = self.width_of(lhs);
        let l = self.compile_expr_hinted(lhs, Some(w))?;
        Ok(match op {
            BinOp::Shl => format!("tail(shl({l}, {n}), {n})"),
            BinOp::Shr => format!("pad(shr({l}, {n}), {w})"),
            _ => unreachable!("compile_shift only called for Shl/Shr"),
        })
    }
}

fn is_state_write(ast: &Ast, res: &Resolution, lhs: ExprId) -> bool {
    match ast.expr(lhs) {
        Expr::Ident(_) => res
            .expr_defs
            .get(&lhs)
            .is_some_and(|d| res.def(*d).kind.is_state()),
        Expr::Bracket { callee, .. } => res
            .expr_defs
            .get(callee)
            .is_some_and(|d| res.def(*d).kind.is_state()),
        Expr::Field { base, .. } => res
            .expr_defs
            .get(base)
            .is_some_and(|d| res.def(*d).kind.is_state()),
        _ => false,
    }
}

fn contains_guard(ast: &Ast, stmt: StmtId) -> bool {
    match ast.stmt(stmt) {
        Stmt::Expr(e) => matches!(ast.expr(*e), Expr::Guard(_)),
        Stmt::If {
            then_body,
            else_body,
            ..
        } => {
            then_body.iter().any(|s| contains_guard(ast, *s))
                || else_body
                    .as_ref()
                    .is_some_and(|b| b.iter().any(|s| contains_guard(ast, *s)))
        }
        Stmt::While { body, .. } => body.iter().any(|s| contains_guard(ast, *s)),
        _ => false,
    }
}
