//! FIRRTL text emission (FIRRTL version 4.0.0, `firtool`-compatible).
//! Front end only, per DESIGN.md: no backend, emit an existing IR as
//! text. This pass turns the scheduler's derived conflict/urgency data
//! into the "guarded atomic rule -> plain synchronous hardware" step —
//! the payoff of the whole design: nobody writes a mux or a `when`
//! chain by hand.
//!
//! Scope, v0. Each is an explicit error, not a silent skip:
//! - Exactly one `module` in the file. No submodule instancing yet.
//! - No `fifo` state and no calls to user `fn`/`spec`/`impl` items —
//!   both need machinery (FIFO synthesis, inlining/instantiation) this
//!   pass doesn't build yet.
//! - No `<suspends>` rules: run `lower::plan`/`render` first. This pass
//!   only lowers "guarded atomic rule" to hardware, not "cycle-crossing
//!   rule" to guarded atomic rules — that is `lower`'s job.
//! - A rule's guards (`expr?`) must all appear before any state write,
//!   and not nested in `if`/`while`: DESIGN.md's failure = abort-the-
//!   whole-rule only lowers to "AND all guards into one `when`" when no
//!   write could have already committed before a guard is checked.
//! - Expression surface: identifiers, integer literals, `+`/`-`
//!   (modular, matching the type checker) and comparisons, memory
//!   indexing. No calls, fields, shifts, multiply, bit-select, or
//!   unary negate yet — exactly what the SUBLEQ and Rmw examples need,
//!   nothing hypothetical beyond it.
//!
//! Memories get one reader port per static read site (not one shared
//! port): the scheduler treats read-read as free, which is only sound
//! in hardware if reads never contend for a port. Writers share one
//! port, since the scheduler already serializes all writers (and every
//! writer conflicts with every reader of the same array in v0), so
//! `read-under-write` is never exercised — it is set to `undefined`.

use crate::ast::{Ast, BinOp, Expr, ExprId, Item, ItemId, Stmt, StmtId};
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
    let modules: Vec<ItemId> = ast
        .roots
        .iter()
        .copied()
        .filter(|id| matches!(ast.item(*id), Item::Module { .. }))
        .collect();
    if modules.len() != 1 {
        return Err(vec![EmitError {
            span: 0..0,
            message: format!(
                "FIRRTL emission needs exactly one module in the file (v0 restriction), found {}",
                modules.len()
            ),
        }]);
    }
    let module = modules[0];
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
        types,
        errors: Vec::new(),
        read_ports: HashMap::new(),
    };

    let mut regs = Vec::new();
    let mut mems = Vec::new();
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
                regs.push((name.text.clone(), w, init));
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
                cx.error(
                    ast.item_spans[id.0 as usize].clone(),
                    format!(
                        "`{}` is a fifo; FIRRTL emission does not synthesize fifo hardware yet \
                         (v0 restriction)",
                        name.text
                    ),
                );
            }
            Item::Rule { name, body, .. } => {
                let still_suspends = fx.sigs.get(id).is_some_and(|s| s.suspends)
                    || body.iter().any(|s| matches!(ast.stmt(*s), Stmt::Tick));
                if still_suspends {
                    cx.error(
                        ast.item_spans[id.0 as usize].clone(),
                        format!(
                            "`{}` is still a <suspends> rule with `tick`; run suspends \
                             lowering first (lower::plan + render), then emit the result",
                            name.text
                        ),
                    );
                    continue;
                }
                rules.push(*id);
            }
            Item::Fn { .. } | Item::Schedule { .. } => {}
            Item::Module { name, .. } => {
                cx.error(
                    ast.item_spans[id.0 as usize].clone(),
                    format!(
                        "nested module `{}` is not supported (v0 restriction)",
                        name.text
                    ),
                );
            }
        }
    }

    let Some(group) = sched.groups.iter().find(|g| g.module == Some(module)) else {
        if !cx.errors.is_empty() {
            return Err(cx.errors);
        }
        return Ok(header(&mod_name.text, &regs, &mems, &HashMap::new(), ""));
    };

    // Assign one reader port per static mem-read site, across all rules,
    // before compiling any expression (a read site's port name must be
    // known wherever it is later referenced).
    for rule in &rules {
        let body = rule_body(ast, *rule);
        cx.collect_read_sites(&body);
    }

    // Guards must all precede any state write, and not be nested.
    // Memory writes must stay top-level (register writes may nest in
    // if/else — SUBLEQ's branch does — but mem writes aren't threaded
    // through a mux yet, so a nested one is an explicit error).
    for rule in &rules {
        cx.check_guard_placement(*rule);
        let body = rule_body(ast, *rule);
        if let Some(span) = find_nested_mem_write(ast, &body) {
            cx.error(
                span,
                "a memory write nested in if/while is not yet supported in FIRRTL \
                 emission (v0 restriction); only a register write may be conditional"
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
    for (rank, rule) in group.order.iter().enumerate() {
        let rule_name = item_name(ast, *rule);
        let signal = format!("fires_{rule_name}");
        let guard = cx.compile_guard(*rule);
        let mut expr = guard;
        for conflict in &group.conflicts {
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
            let other_rank = group.order.iter().position(|r| *r == other).unwrap();
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
        ordered.sort_by_key(|r| std::cmp::Reverse(group.order.iter().position(|x| x == r)));
        for rule in ordered {
            let (addr, data) = cx.write_target(rule, mem_name, *elem_width);
            let f = &fires_name[&rule];
            let _ = writeln!(mem_body, "    when {f} :");
            let _ = writeln!(mem_body, "      connect {mem_name}.{port}.addr, {addr}");
            let _ = writeln!(mem_body, "      connect {mem_name}.{port}.data, {data}");
        }
    }

    // Registers: same priority-mux pattern; unwritten paths hold by
    // FIRRTL's default register semantics, no explicit else needed. A
    // register written on only one side of an if/else (SUBLEQ's branch:
    // `if r <= 0 { pc := c } else { pc := pc + 3 }`) gets its value
    // threaded through as a `mux`, not silently dropped for not being a
    // top-level assignment.
    let mut reg_body = String::new();
    for (reg_name, width, _) in &regs {
        let mut values: Vec<(ItemId, String)> = Vec::new();
        for rule in &rules {
            let body = rule_body(ast, *rule);
            if let Some(v) = cx.reg_value_in_stmts(&body, reg_name, *width) {
                values.push((*rule, v));
            }
        }
        if values.is_empty() {
            continue;
        }
        values.sort_by_key(|(r, _)| std::cmp::Reverse(group.order.iter().position(|x| x == r)));
        for (rule, value) in values {
            let f = &fires_name[&rule];
            let _ = writeln!(reg_body, "    when {f} :");
            let _ = writeln!(reg_body, "      connect {reg_name}, {value}");
        }
    }

    if !cx.errors.is_empty() {
        return Err(cx.errors);
    }

    let mut body = String::new();
    body.push_str(&fires_body);
    body.push('\n');
    body.push_str(&mem_body);
    body.push('\n');
    body.push_str(&reg_body);
    Ok(header(&mod_name.text, &regs, &mems, &mem_ports, &body))
}

fn header(
    name: &str,
    regs: &[(String, u64, u64)],
    mems: &[(String, u64, u64)],
    mem_ports: &HashMap<String, (Vec<String>, Option<String>)>,
    body: &str,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "FIRRTL version 4.0.0");
    let _ = writeln!(out, "circuit {name} :");
    let _ = writeln!(out, "  public module {name} :");
    let _ = writeln!(out, "    input clock : Clock");
    let _ = writeln!(out, "    input reset : UInt<1>");
    out.push('\n');
    for (n, w, init) in regs {
        let _ = writeln!(
            out,
            "    regreset {n} : UInt<{w}>, clock, reset, UInt<{w}>({init})"
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

fn is_ident_named(ast: &Ast, res: &Resolution, id: ExprId, name: &str) -> bool {
    matches!(ast.expr(id), Expr::Ident(_))
        && res.expr_defs.get(&id).is_some_and(|d| {
            let d = res.def(*d);
            d.name == name && d.kind == DefKind::Reg
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

struct Emitter<'a> {
    ast: &'a Ast,
    res: &'a Resolution,
    types: &'a Types,
    errors: Vec<EmitError>,
    /// Each static mem-read expression -> its assigned reader port name.
    read_ports: HashMap<ExprId, String>,
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

    /// A guard (`expr?`) may only appear before any state write in the
    /// same rule, and only at the top level.
    fn check_guard_placement(&mut self, rule: ItemId) {
        let body = rule_body(self.ast, rule);
        let mut seen_write = false;
        for stmt in &body {
            match self.ast.stmt(*stmt).clone() {
                Stmt::Assign { lhs, .. } => {
                    if is_state_write(self.ast, self.res, lhs) {
                        seen_write = true;
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
                }
                _ => {}
            }
        }
    }

    fn compile_guard(&mut self, rule: ItemId) -> String {
        let body = rule_body(self.ast, rule);
        let mut conds = Vec::new();
        for stmt in &body {
            if let Stmt::Expr(e) = self.ast.stmt(*stmt)
                && let Expr::Guard(inner) = self.ast.expr(*e)
            {
                conds.push(
                    self.compile_expr(*inner)
                        .unwrap_or_else(|_| "UInt<1>(1)".to_string()),
                );
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
                        "a loop in an emitted rule body is not supported (suspends \
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
        match self.ast.expr(id).clone() {
            Expr::Ident(_) => {
                let def = self.res.expr_defs.get(&id).copied();
                match def.map(|d| self.res.def(d).clone()) {
                    Some(d) if matches!(d.kind, DefKind::Reg | DefKind::Local) => Ok(d.name),
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
            Expr::Bracket { .. } => {
                self.error(
                    self.ast.expr_spans[id.0 as usize].clone(),
                    "this indexing form is not yet supported in FIRRTL emission (v0 \
                     restriction: only memory reads on a plain identifier)"
                        .to_string(),
                );
                Err(())
            }
            _ => {
                self.error(
                    self.ast.expr_spans[id.0 as usize].clone(),
                    "this expression form is not yet supported in FIRRTL emission (v0 \
                     restriction: identifiers, integers, +/-, comparisons, and memory \
                     reads only)"
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
        let known_width = |types: &Types, e: ExprId| {
            types.expr_tys.get(&e).and_then(|t| match t {
                Ty::Bits(Width::Known(w)) => Some(*w),
                _ => None,
            })
        };
        let hint = if matches!(op, BinOp::Add | BinOp::Sub) {
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
                     restriction: +, -, and comparisons only)"
                        .to_string(),
                );
                return Err(());
            }
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
