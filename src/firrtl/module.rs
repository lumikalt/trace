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
        locals_snapshots: Vec::new(),
        current_pos: 0,
    };

    // `(match_name, emit_name, width, init)`. For a plain `reg`, both
    // names are the user's; for an `output`, `match_name` is the port
    // name (what rule bodies write) and `emit_name` is its internal
    // backing register (see the `Item::Output` arm below).
    let mut regs: Vec<(String, String, u64, u64)> = Vec::new();
    let mut mems = Vec::new();
    // `(fifo_name, width, depth)`.
    let mut fifos: Vec<(String, u64, u64)> = Vec::new();
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
                let Some(Ty::Fifo { elem, depth }) = cx.state_width(def) else {
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
                if depth == 0 {
                    cx.error(
                        ast.item_spans[id.0 as usize].clone(),
                        format!("`{}` needs a depth of at least 1", name.text),
                    );
                    continue;
                }
                if depth == 1 {
                    // A depth-1 buffer: one data register, one valid
                    // bit, both internally named to avoid colliding
                    // with a user identifier (same `__`-prefix
                    // convention as `__out_x` and lower.rs's
                    // `__cont_x`). Kept as its own case (rather than a
                    // degenerate N=1 instance of the general one-slot-
                    // array-plus-head-plus-count shape below) so the
                    // by-far-most-common depth emits byte-identical
                    // FIRRTL to before depth support existed.
                    regs.push((
                        fifo_valid_name(&name.text),
                        fifo_valid_name(&name.text),
                        1,
                        0,
                    ));
                    regs.push((fifo_data_name(&name.text), fifo_data_name(&name.text), w, 0));
                } else {
                    // A depth-N circular buffer: N data-slot registers
                    // plus `head`/`count` pointer registers (`tail` is
                    // derived, `head + count` wrapped — not stored).
                    // See fifo.rs's module doc comment: this shape was
                    // hand-verified against a real firtool+Icarus
                    // simulation (depth 3, chosen non-power-of-2 to
                    // stress wraparound) before being ported here.
                    for i in 0..depth {
                        let slot = fifo_slot_name(&name.text, i);
                        regs.push((slot.clone(), slot, w, 0));
                    }
                    let head = fifo_head_name(&name.text);
                    regs.push((head.clone(), head, clog2(depth).max(1), 0));
                    let count = fifo_count_name(&name.text);
                    regs.push((count.clone(), count, count_width(depth), 0));
                }
                fifos.push((name.text.clone(), w, depth));
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
        cx.collect_read_sites(&body, *rule);
    }

    // Guards must all precede any state write, and not be nested.
    // Register, instance-port, and memory writes may all nest in
    // if/else — SUBLEQ's branch does, for a register — each threaded
    // through a `mux` (see `reg_value_in_stmts`/
    // `inst_port_value_in_stmts`/`mem_write_in_stmts`).
    for rule in &rules {
        cx.check_guard_placement(*rule);
        cx.check_writing_call_positions(*rule);
        cx.check_failing_call_positions(*rule);
        cx.check_fifo_op_positions(*rule);
        cx.check_logic_args(*rule);
        cx.check_fifo_op_counts(*rule);
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
    // An address expression may reference the OWNING rule's own locals
    // (`x := addr  out := m[x]`), so `enter_rule` must be re-entered
    // for that specific rule before compiling it — this loop runs
    // AFTER the fires loop above, whose own `enter_rule` calls leave
    // `cx.locals` pointing at whichever rule was entered LAST, which is
    // wrong for every other rule's own read sites (a real, previously
    // latent bug: a local-addressed read in a non-last rule hit "cannot
    // find this local's binding"). Also hint the address with the
    // mem's own address width — a bare-literal address (`x := 5`) has
    // no width of its own to fall back on otherwise.
    let mut mem_body = String::new();
    for (site_expr, (port, owning_rule, owning_stmt)) in cx.read_ports.clone() {
        let Expr::Bracket { callee, args } = ast.expr(site_expr).clone() else {
            continue;
        };
        let mem_name = match ast.expr(callee) {
            Expr::Ident(n) => n.clone(),
            _ => continue,
        };
        let addr_w = mems
            .iter()
            .find(|(n, _, _)| n == &mem_name)
            .map(|(_, _, depth)| clog2(*depth).max(1));
        cx.enter_rule(owning_rule);
        cx.set_pos(owning_rule, owning_stmt);
        let addr = cx.compile_expr_hinted(args[0], addr_w).unwrap_or_default();
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
                    .mem_write_in_stmts(&body, rule, mem_name, *elem_width, addr_w)
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

    // Fifos. At most one rule can touch a given fifo per cycle — every
    // fifo op reads+writes it (effects.rs), so every touching rule
    // conflicts with every other, exactly like mem writers above; same
    // priority-mux pattern, though only one `when` can ever actually be
    // live per fifo. A rule may enqueue AND dequeue the SAME fifo (a
    // pass-through — see fifo.rs's module doc comment): `Deq[]`'s own
    // value already reads the pre-edge state before these connects take
    // effect, the same "reads see the old value, connects land for next
    // cycle" register semantics used everywhere else in this emitter.
    // `(rule, Some(the Enq op — direct or via a callee), saw a dequeue)`.
    // `rule_fifo_ops` (fifo.rs) is the single enumerator every fifo-touch
    // question in this emitter routes through — it already finds an Enq/
    // Deq reached through exactly one failing-callee call, not just a
    // rule's own top-level statements, and `check_fifo_op_counts` has
    // already confirmed (before this ever runs) that a rule enqueues at
    // most once and dequeues at most once per fifo, however it's spread
    // across a direct op and a callee's own op.
    type FifoTouch = (ItemId, Option<RuleFifoOp>, bool);
    let mut fifo_body = String::new();
    for (fifo_name, width, depth) in &fifos {
        let mut touching: Vec<FifoTouch> = Vec::new();
        for rule in &rules {
            let mut enq: Option<RuleFifoOp> = None;
            let mut saw_deq = false;
            for op in cx.rule_fifo_ops(*rule) {
                if &op.fifo != fifo_name {
                    continue;
                }
                if op.is_enq {
                    enq = Some(op);
                } else {
                    saw_deq = true;
                }
            }
            if enq.is_some() || saw_deq {
                touching.push((*rule, enq, saw_deq));
            }
        }
        if touching.is_empty() {
            continue;
        }
        touching.sort_by_key(|(r, ..)| std::cmp::Reverse(order.iter().position(|x| x == r)));
        if *depth == 1 {
            let valid = fifo_valid_name(fifo_name);
            let data = fifo_data_name(fifo_name);
            for (rule, enq, _saw_deq) in touching {
                cx.enter_rule(rule);
                let f = &fires_name[&rule];
                let _ = writeln!(fifo_body, "    when {f} :");
                if let Some(enq_op) = enq {
                    cx.set_pos(rule, enq_op.stmt);
                    let value = cx
                        .compile_fifo_op_value(&enq_op, *width)
                        .unwrap_or_default();
                    let _ = writeln!(fifo_body, "      connect {valid}, UInt<1>(1)");
                    let _ = writeln!(fifo_body, "      connect {data}, {value}");
                } else {
                    let _ = writeln!(fifo_body, "      connect {valid}, UInt<1>(0)");
                }
            }
        } else {
            cx.emit_fifo_depth_n(
                &mut fifo_body,
                fifo_name,
                *width,
                *depth,
                &touching,
                &fires_name,
            );
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
            if let Some(v) = cx.reg_value_in_stmts(&body, *rule, match_name, *width) {
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
                if let Some(v) = cx.inst_port_value_in_stmts(&body, *rule, inst_name, port_name, w)
                {
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
    /// A depth-N (N>1) fifo's state-transition emission: N data-slot
    /// registers plus `head`/`count` pointers. Design hand-verified
    /// against a real firtool+Icarus simulation of a depth-3 circuit
    /// (see fifo.rs's module doc comment) before being ported here —
    /// caught a real bug in composing the Enq/Deq `count` update (a
    /// combined Enq+Deq must net to unchanged, not lose the Enq's
    /// credit by branching off the SAME pre-update `count` twice).
    ///
    /// `tail` (the next write position) is derived, not stored: `head +
    /// count` wrapped back into `[0, depth)`. It's computed once here,
    /// as plain top-level nodes shared by every touching rule's own
    /// `when` block below, since it depends only on the `head`/`count`
    /// registers' current (pre-edge) values, not on which rule fires.
    #[allow(clippy::too_many_arguments)]
    fn emit_fifo_depth_n(
        &mut self,
        out: &mut String,
        fifo_name: &str,
        width: u64,
        depth: u64,
        touching: &[(ItemId, Option<RuleFifoOp>, bool)],
        fires_name: &HashMap<ItemId, String>,
    ) {
        let head = fifo_head_name(fifo_name);
        let count = fifo_count_name(fifo_name);
        let head_w = clog2(depth).max(1);
        let count_w = count_width(depth);
        let pos = format!("__fifo_{fifo_name}_pos");
        let wpos = format!("__fifo_{fifo_name}_wpos");
        let head_p1 = format!("__fifo_{fifo_name}_head_p1");
        let _ = writeln!(out, "    node {pos} = add({head}, {count})");
        let _ = writeln!(
            out,
            "    node {wpos} = mux(geq({pos}, UInt<{count_w}>({depth})), \
             sub({pos}, UInt<{count_w}>({depth})), {pos})"
        );
        let _ = writeln!(
            out,
            "    node {head_p1} = mux(eq({head}, UInt<{head_w}>({})), UInt<{head_w}>(0), \
             tail(add({head}, UInt<{head_w}>(1)), 1))",
            depth - 1
        );
        for (rule, enq, saw_deq) in touching {
            self.enter_rule(*rule);
            let f = &fires_name[rule];
            let _ = writeln!(out, "    when {f} :");
            if let Some(enq_op) = enq {
                self.set_pos(*rule, enq_op.stmt);
                let value = self
                    .compile_fifo_op_value(enq_op, width)
                    .unwrap_or_default();
                if !saw_deq {
                    let _ = writeln!(
                        out,
                        "      connect {count}, tail(add({count}, UInt<{count_w}>(1)), 1)"
                    );
                }
                // Combined Enq+Deq: `count` is unchanged (the Enq's +1
                // above is skipped and the Deq's -1 below is skipped
                // too — they'd net to zero anyway, but composing them
                // by branching off the SAME pre-update `count` twice
                // is exactly the bug the hand-verified circuit caught;
                // omitting both connects sidesteps it entirely and
                // holds the register's current value).
                for i in 0..depth {
                    let slot = fifo_slot_name(fifo_name, i);
                    let _ = writeln!(
                        out,
                        "      connect {slot}, mux(eq({wpos}, UInt<{count_w}>({i})), {value}, \
                         {slot})"
                    );
                }
            }
            if *saw_deq {
                let _ = writeln!(out, "      connect {head}, {head_p1}");
                if enq.is_none() {
                    let _ = writeln!(
                        out,
                        "      connect {count}, tail(sub({count}, UInt<{count_w}>(1)), 1)"
                    );
                }
            }
        }
    }

    pub(crate) fn collect_read_sites(&mut self, stmts: &[StmtId], rule: ItemId) {
        for stmt in stmts {
            match self.ast.stmt(*stmt).clone() {
                Stmt::Assign { lhs, rhs } => {
                    self.collect_read_sites_expr(rhs, rule, *stmt);
                    if let Expr::Bracket { args, .. } = self.ast.expr(lhs).clone() {
                        for a in args {
                            self.collect_read_sites_expr(a, rule, *stmt);
                        }
                    }
                }
                Stmt::Expr(e) => self.collect_read_sites_expr(e, rule, *stmt),
                Stmt::Let { init, .. } => self.collect_read_sites_expr(init, rule, *stmt),
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    self.collect_read_sites_expr(cond, rule, *stmt);
                    self.collect_read_sites(&then_body, rule);
                    if let Some(e) = else_body {
                        self.collect_read_sites(&e, rule);
                    }
                }
                _ => {}
            }
        }
    }

    pub(crate) fn collect_read_sites_expr(&mut self, id: ExprId, rule: ItemId, stmt: StmtId) {
        if let Expr::Bracket { callee, .. } = self.ast.expr(id).clone()
            && let Expr::Ident(_) = self.ast.expr(callee)
            && let Some(def) = self.res.expr_defs.get(&callee)
            && self.res.def(*def).kind == DefKind::Mem
        {
            let n = self.read_ports.len();
            self.read_ports.insert(id, (format!("r{n}"), rule, stmt));
            return; // the address sub-expr is compiled, not walked further
        }
        for child in crate::lower::sub_exprs(self.ast, id) {
            self.collect_read_sites_expr(child, rule, stmt);
        }
    }
}
