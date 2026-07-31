//! `emit_module`: the per-module driver. Walks one module's items once to
//! collect regs/mems/fifos/ports/instances/rules, runs the pre-compilation
//! checks (checks.rs), then emits each hardware construct's FIRRTL text in
//! turn — registers, memories, fifos, instances, ports, the priority-mux
//! `fires_*` signals derived from the schedule. `module_block` assembles
//! the final per-module text; `port_bit_width` is a small shared helper.

use super::EmitError;
use super::Emitter;
use super::fifo::*;
use super::module_name;
use super::writes::*;
use crate::ast::{Ast, Expr, ExprId, Item, ItemId, Stmt, StmtId};
use crate::effects::Effects;
use crate::resolve::{DefId, DefKind, Resolution};
use crate::schedule::{Exemption, Schedule};
use crate::types::{Ty, Types, Width};
use std::collections::HashMap;
use std::fmt::Write as _;

/// Emit one module's FIRRTL text (its `public module`/`module` line, ports,
/// declarations, and body) — everything except the `circuit` wrapper,
/// which the caller writes once for the whole file.
// `ast`/`res`/`fx`/`types`/`sched` is the same five-pass-result bundle
// threaded through every stage of this pipeline (see `emit`, `lower::plan`,
// etc.) — bundling them into a struct here alone would be inconsistent
// with that established convention, not an improvement.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_module(
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
        module,
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
    // Register, instance-port, and memory writes may all nest in
    // if/else — SUBLEQ's branch does, for a register — each threaded
    // through a `mux` (see `reg_value_in_stmts`/
    // `inst_port_value_in_stmts`/`mem_write_in_stmts`).
    for rule in &rules {
        cx.check_guard_placement(*rule);
        cx.check_fifo_same_cycle(*rule);
        cx.check_no_reassigned_locals(*rule);
        cx.check_writing_call_positions(*rule);
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
            if conflict.exemption.is_exempted() {
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

    // Both exemption kinds waive the derived stall above (loop just
    // finished). Only `mutually_exclusive` gets a runtime check here —
    // it claims the two rules never both fire, which is checkable by
    // asserting exactly that. `conflict_free` claims the OPPOSITE thing
    // (safe to fire together) and stays trusted, not checked: v0 has no
    // way to prove or check address disjointness (DESIGN.md's tier-3
    // proof, deferred), so there is nothing sound to assert for it —
    // see this module's own doc comment and schedule.rs's `Exemption`.
    // `enable` is gated on `not(reset)` since a rule's own guard may
    // read state that hasn't settled to its real reset value yet on the
    // reset cycle itself, and a spurious fires-both during reset would
    // be a false claim violation, not a real one.
    for (i, conflict) in conflicts.iter().enumerate() {
        if conflict.exemption != Exemption::MutuallyExclusive {
            continue;
        }
        let a_name = item_name(ast, conflict.a);
        let b_name = item_name(ast, conflict.b);
        let a_fires = &fires_name[&conflict.a];
        let b_fires = &fires_name[&conflict.b];
        let _ = writeln!(
            fires_body,
            "    assert(clock, not(and({a_fires}, {b_fires})), not(reset), \"mutually_exclusive \
             claim violated: rule {a_name} and rule {b_name} both fired the same cycle\") : \
             mutually_exclusive_check_{i}"
        );
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

    // Writers: one shared port per mem, enable = OR of (writer fires AND
    // actually writes this cycle), priority-muxed by urgency (most
    // urgent's connect emitted last). A writer's own write may now be
    // conditional (nested in if/else) rather than guaranteed just
    // because the rule fires, so `en` can no longer be a plain OR of
    // `fires` alone — each writer's own write-enable (from
    // `mem_write_in_stmts`) has to factor in too.
    for (mem_name, elem_width, depth) in &mems {
        let writers = writers_of(ast, res, &rules, mem_name);
        if writers.is_empty() {
            continue;
        }
        let port = format!("w_{mem_name}");
        if let Some((_, writer)) = mem_ports.get_mut(mem_name) {
            *writer = Some(port.clone());
        }
        let addr_w = clog2(*depth).max(1);
        // Compute each writer's (write-enable, addr, data) up front — the
        // `en` net needs every writer's own enable, and the priority
        // loop below needs the same values again, so they're only
        // compiled once per writer rather than twice.
        let per_writer: Vec<(ItemId, String, String, String)> = writers
            .iter()
            .map(|&rule| {
                cx.enter_rule(rule);
                let body = rule_body(ast, rule);
                let (wrote, addr, data) = cx
                    .mem_write_in_stmts(&body, mem_name, *elem_width, addr_w)
                    .unwrap_or_else(|| {
                        (
                            "UInt<1>(0)".to_string(),
                            format!("UInt<{addr_w}>(0)"),
                            format!("UInt<{elem_width}>(0)"),
                        )
                    });
                (rule, wrote, addr, data)
            })
            .collect();
        let en = per_writer
            .iter()
            .map(|(rule, wrote, ..)| format!("and({}, {wrote})", fires_name[rule]))
            .reduce(|a, b| format!("or({a}, {b})"))
            .unwrap();
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
        // among non-exempted conflicting writers. The addr/data
        // connected here are already correctly muxed down to their
        // rule's own default (0) on any path that doesn't actually
        // write, by `mem_write_in_stmts` itself — no extra gating needed
        // beyond the existing `when {f}`.
        let mut ordered = per_writer;
        ordered.sort_by_key(|(rule, ..)| std::cmp::Reverse(order.iter().position(|x| x == rule)));
        for (rule, _wrote, addr, data) in ordered {
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

pub(crate) fn port_bit_width(ty: &Ty) -> Option<u64> {
    match ty {
        Ty::Bits(Width::Known(w)) => Some(*w),
        _ => None,
    }
}

// Nine genuinely distinct pieces of one module's assembled text, each
// used once here; a bundling struct would exist solely to satisfy this
// lint at this one call site, not to clarify anything.
#[allow(clippy::too_many_arguments)]
pub(crate) fn module_block(
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

impl<'a> Emitter<'a> {
    pub(crate) fn collect_read_sites(&mut self, stmts: &[StmtId]) {
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

    pub(crate) fn collect_read_sites_expr(&mut self, id: ExprId) {
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
}
