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
//!   `reg`/`mem`/`fifo`/`in`/`out`/`inst` declared in its OWN
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
//!   `let` bindings and state writes, then either a trailing
//!   `return <expr>` or an `if`/`else` whose branches both recurse into
//!   that same shape (mandatory `else`, folded into a `mux`) — no loops
//!   or guards/fifo ops anywhere in the callee. A callee's own body MAY
//!   call another `fn`/`impl` (composition, not just a leaf value
//!   computation), used as a `let`'s init, the return expression, a
//!   state write's own RHS, or a bare statement (its return value
//!   discarded — useful for a state-writing side effect alone) — as
//!   long as it doesn't form a call CYCLE, direct or indirect (a STATIC
//!   property of which functions' own bodies name which others,
//!   `find_call_cycle`). A state write, whether the callee's own direct
//!   write or one reached transitively through a nested call, only
//!   reaches the emitted hardware when it sits in a bare statement or
//!   the whole RHS of `:=` — `check_writing_call_positions` enforces
//!   this at BOTH the rule level and (via `validate_call`, its single
//!   choke point) every callee body a call reaches, rejecting anywhere
//!   else explicitly (a `let`, an argument, a larger expression) rather
//!   than silently dropping the write; `callee_reg_write`/
//!   `callee_port_write` (the write-hunt into a callee's own body) then
//!   recurse into exactly those same two positions to actually find it,
//!   arbitrarily many calls deep. A conditional write is only inlinable
//!   as a bare statement — if its return value is ALSO used, the
//!   `if`/`else` must then be in TAIL position (same restriction as
//!   the return-value-only case). A `spec` call cannot reach this pass
//!   at all (effects.rs already rejects it outside spec-only code). A
//!   call to the builtin `prio` (a fixed-priority encoder) is separately
//!   supported too, as a `mux` chain, and — unlike a call to another
//!   user `fn`/`impl` — doesn't disqualify a callee from inlining; every
//!   other builtin remains an explicit, still-unsupported gap.
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
//!   computed bit-select bounds, or logical `not` yet.
//!
//! Fifos default to depth 1 (one data register plus one valid bit);
//! `[depth]elem_ty` declares a deeper one (N data-slot registers plus
//! `head`/`count` pointers — see fifo.rs's module doc comment).
//! `Deq[]` succeeds iff non-empty; `Enq[x]` succeeds iff non-full — the
//! two failure conditions fold into the rule's guard exactly like an
//! explicit `?`. A rule MAY both `Enq` and `Deq` the same fifo in one
//! cycle (a pass-through: `Deq` reads the pre-edge data, `Enq` writes
//! the new data for next cycle, fill level unchanged) — fifo.rs's
//! module doc comment covers the combined guard and update this needs.
//!
//! Memories get one reader port per static read site (not one shared
//! port): the scheduler treats read-read as free, which is only sound
//! in hardware if reads never contend for a port. Writers share one
//! port, since the scheduler already serializes all writers.
//!
//! `read-under-write => old`: a read and a write to the SAME address the
//! SAME cycle must see the mem's PRE-EDGE contents, not the write landing
//! mid-cycle — this is exactly the pre-edge-read invariant every register
//! already has (DESIGN.md's "Scheduling"), stated for mems rather than
//! left to a tool's own default. This IS exercised by ordinary,
//! unexempted designs, not just a `conflict_free` claim turning out
//! wrong: two independent-address accesses in the SAME rule
//! (`examples/mem_write_branch.tr`'s `m[addr] := data` / `m[read_addr]`)
//! can coincide at runtime with no conflict pair and no annotation
//! involved at all. Costs nothing at `read-latency => 0` (firtool already
//! lowers same-cycle read/write here to a combinational read against the
//! write's own nonblocking assign — confirmed byte-identical Verilog
//! against `undefined`); becomes load-bearing only if `read-latency` is
//! ever raised above 0 (`module.rs`'s own mem-emission site).

use crate::ast::{Ast, Expr, ExprId, Item, ItemId, StmtId};
use crate::effects::Effects;
use crate::lexer::Span;
use crate::resolve::{DefId, Resolution};
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
    // Same, for `extmodule` targets — kept as its own map rather than
    // merged into `item_of_module_def` above: an extmodule is never a
    // candidate for "the top" and never itself walked by `visit_module`
    // (it has no body/rules/`inst`s of its own), only ever a LEAF `inst`
    // target, so keeping the two maps separate means `inst_targets`
    // (which drives top-detection and the transitive walk) stays
    // Module-only for free, with no extra filtering needed there.
    let item_of_extmodule_def: HashMap<DefId, ItemId> = all_extmodules(ast)
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
        match emit_module(
            ast,
            res,
            fx,
            types,
            sched,
            m,
            m == top,
            &item_of_module_def,
            &item_of_extmodule_def,
        ) {
            Ok(text) => blocks.push(text),
            Err(errs) => all_errors.extend(errs),
        }
    }
    if !all_errors.is_empty() {
        return Err(all_errors);
    }

    // Every extmodule any emitted module actually `inst`s, each declared
    // exactly once (dedup by `ItemId`, order doesn't matter — FIRRTL
    // doesn't care about declaration order). Unlike an ordinary module, an
    // extmodule contributes no rules/state of its own to walk, so it
    // never goes through `emit_module` — just its bare port-list
    // declaration (see `emit_extmodule`).
    let mut extmodule_ids: Vec<ItemId> = Vec::new();
    let mut seen_extmodules: std::collections::HashSet<ItemId> = std::collections::HashSet::new();
    for &m in &to_emit {
        for target in inst_extmodule_targets(ast, res, m, &item_of_extmodule_def) {
            if seen_extmodules.insert(target) {
                extmodule_ids.push(target);
            }
        }
    }
    let extmodule_blocks: Vec<String> = extmodule_ids
        .iter()
        .map(|&m| emit_extmodule(ast, res, types, m))
        .collect();

    let mut out = String::new();
    let _ = writeln!(out, "FIRRTL version 4.0.0");
    let _ = writeln!(out, "circuit {} :", module_name(ast, top));
    for block in extmodule_blocks {
        out.push_str(&block);
        out.push('\n');
    }
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

/// The extmodules a module `m` directly instantiates via `inst` — the
/// same shape as `inst_targets` above, but resolved against `item_of_
/// extmodule_def` instead. Kept separate rather than merging the two
/// maps and filtering here: `inst_targets` drives top-detection and the
/// transitive walk, which must stay Module-only (see `item_of_extmodule_
/// def`'s own doc comment in `emit`).
fn inst_extmodule_targets(
    ast: &Ast,
    res: &Resolution,
    m: ItemId,
    item_of_extmodule_def: &HashMap<DefId, ItemId>,
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
                .and_then(|d| item_of_extmodule_def.get(d))
                .copied(),
            _ => None,
        })
        .collect()
}

/// Every `Item::ExtModule` in the file, regardless of lexical nesting —
/// same reasoning as `all_modules` (FIRRTL has no nested-module concept,
/// only cross-references via `inst X of Y`), plus `extmodule` declares no
/// body/rules/`inst`s of its own to recurse into further.
fn all_extmodules(ast: &Ast) -> Vec<ItemId> {
    let mut out = Vec::new();
    let mut worklist: Vec<ItemId> = ast.roots.clone();
    while let Some(id) = worklist.pop() {
        match ast.item(id) {
            Item::Module { items, .. } => worklist.extend(items.iter().copied()),
            Item::ExtModule { .. } => out.push(id),
            _ => {}
        }
    }
    out
}

/// An extmodule's bare FIRRTL declaration: its own port list and a
/// `defname` line, nothing else — no body, no clock/reset (v0
/// restriction: an extmodule's ports are plain `in`/`out`/`io` only, see
/// `ast::ExtPort`'s doc comment; a blackbox needing a clock declares one
/// as an ordinary port and gets it wired like any other instance input).
/// The referenced `.v` implementation is never mentioned here — confirmed
/// by hand-lowering one through firtool: FIRRTL text has no linkage to it
/// at all, entirely a downstream build/simulation concern.
///
/// Reads widths from `types.module_ports` (already computed and shape-
/// checked by `collect_module_ports`) rather than re-deriving them from
/// the raw `ast::ExtPort` list — one source of truth for a port's width,
/// same as every other emission site in this module.
fn emit_extmodule(ast: &Ast, res: &Resolution, types: &Types, m: ItemId) -> String {
    let Item::ExtModule { name, .. } = ast.item(m) else {
        unreachable!("all_extmodules only ever collects Item::ExtModule");
    };
    let mut out = String::new();
    let _ = writeln!(out, "  extmodule {} :", name.text);
    let empty = Vec::new();
    let ports = res
        .item_defs
        .get(&m)
        .and_then(|d| types.module_ports.get(d))
        .unwrap_or(&empty);
    for (pname, kind, ty) in ports {
        let w = port_bit_width(ty).unwrap_or(1);
        let (kw, ty_text) = match kind {
            crate::resolve::DefKind::Input => ("input", format!("UInt<{w}>")),
            crate::resolve::DefKind::Io => ("output", format!("Analog<{w}>")),
            _ => ("output", format!("UInt<{w}>")),
        };
        let _ = writeln!(out, "    {kw} {pname} : {ty_text}");
    }
    let _ = writeln!(out, "    defname = {}", name.text);
    out
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
        Item::ExtModule { name, .. } => &name.text,
        _ => "?",
    }
}

// Split by responsibility, not by size: `module` (the per-module driver),
// `checks` (pre-compilation validation), `fifo` (Enq/Deq detection and
// naming), `writes` (state-write threading through if/else), `calls`
// (inlining a user fn/impl or the builtin `prio`), `expr` (the generic
// expression compiler). Each file's own doc comment says more.
mod calls;
mod checks;
mod expr;
mod fifo;
mod module;
mod writes;

use module::*;

struct Emitter<'a> {
    ast: &'a Ast,
    res: &'a Resolution,
    fx: &'a Effects,
    types: &'a Types,
    /// The module currently being emitted — a call's own state-reaching
    /// reads/writes are checked against this, not the callee's
    /// declaration site (see `compile_call`).
    module: ItemId,
    errors: Vec<EmitError>,
    /// Each static mem-read expression -> its assigned reader port name,
    /// plus the rule and enclosing (possibly if/else-nested) statement
    /// it was found in — needed to `enter_rule`/`set_pos` the right
    /// context before compiling its address expression, since a read
    /// site's address is compiled separately from, and after, the rule
    /// body it lexically belongs to (reads are wired unconditionally
    /// across the whole module; see module.rs's reader-port loop).
    read_ports: HashMap<ExprId, (String, ItemId, StmtId)>,
    /// Output port name -> its internal backing register name. Reading
    /// an output inside a rule (`sum := sum + inc`) must see the
    /// register, not the port (a FIRRTL output port is drive-only from
    /// inside its own module in the shape this emitter produces).
    output_regs: HashMap<String, String>,
    /// A CALLEE body's own params/`let`s (see `calls.rs`/`callee_reg_write`/
    /// `callee_port_write`): single binding per `DefId`, save/restored
    /// around a call for reentrancy safety. Unrelated to a RULE's own
    /// top-level locals — see `locals_snapshots` below for those. A
    /// `DefId` never appears in both maps, so `compile_expr`'s Ident
    /// case checks `locals_snapshots` first and falls back to this one.
    locals: HashMap<DefId, ExprId>,
    /// A rule's own top-level local bindings, each ALREADY COMPILED to
    /// FIRRTL text and snapshotted per top-level statement position:
    /// `locals_snapshots[i]` is every local's value as established by
    /// statements STRICTLY BEFORE `rule_body(rule)[i]` — i.e. what that
    /// statement (or anything nested inside it, e.g. an if/else branch)
    /// should see. Rebuilt fresh per rule by `enter_rule`, which walks
    /// the body in program order and compiles each local's own RHS
    /// EAGERLY (not lazily, unlike `locals` above) the moment it's
    /// bound, using whatever was already established so far — the only
    /// way a REASSIGNED local resolves correctly: a lazy, ExprId-keyed
    /// single binding (the old design) can't distinguish "read before
    /// the second assignment" from "read after" once compilation is
    /// deferred past `enter_rule`'s own pass. `current_pos` (below)
    /// selects which snapshot is active; `set_pos` computes it from
    /// whichever statement is currently being compiled. See DESIGN.md's
    /// "Reassigned locals" section for the full design story.
    locals_snapshots: Vec<HashMap<DefId, String>>,
    /// Index into `locals_snapshots` currently in effect — set by
    /// `set_pos` before compiling any expression that might reference a
    /// rule-local, consulted by `compile_expr`'s Ident case. Stays fixed
    /// across a nested call's own param/body compilation (calls never
    /// touch this field), which is exactly what makes a call argument
    /// referencing a rule-local resolve correctly: the call's own
    /// compile happens "at" whatever position the caller set.
    current_pos: usize,
    /// `if let NAME = opt? { ... }`'s own bound `NAME` -> `opt`'s ExprId
    /// (the Guard's own inner, NOT yet chased down to `.data` — each
    /// reader does that itself via `struct_field_path`, since `opt` may
    /// itself be a chained field, e.g. `frame.maybe?`). Active only
    /// while compiling that one `then_body`: every write-threading walk
    /// that recurses into a `Stmt::IfLet`'s `then_body` inserts before
    /// and removes right after (a fresh `DefId` per binder, so a plain
    /// insert/remove is sound even for nested `if let`s — no prior value
    /// to save/restore the way `calls.rs`'s param save/restore needs).
    /// `compile_expr_hinted`'s `Ident` case checks this before falling
    /// back to `locals_snapshots`/`locals` — deliberately a THIRD,
    /// separate map rather than reusing `locals` (which holds an
    /// ExprId whose compiled value the reader wants directly; this one
    /// instead needs `.data` appended via `struct_field_path`, a
    /// different shape) or `locals_snapshots` (which never even sees
    /// this def — `enter_rule` only scans the RULE's own top-level
    /// statements, and an `if let` is always nested inside SOME then/
    /// else body, so its own binding statement is never a top-level
    /// entry `enter_rule`'s loop would visit).
    if_let_binds: HashMap<DefId, ExprId>,
}

impl<'a> Emitter<'a> {
    /// A single call site can be validated more than once — `validate_call`
    /// runs once for a call's return value (`compile_call`) and again for
    /// each register/port whose write-hunt reaches it (`call_writes_reg`/
    /// `call_writes_port`) — so any error it emits (a bad write position,
    /// a call cycle, a module-boundary violation, ...) would otherwise be
    /// pushed once per path, all with the identical span and message.
    /// Deduping here, at the single choke point every `error` call goes
    /// through, covers that whole class in one place rather than each
    /// caller re-deriving "have I already validated this."
    pub(crate) fn error(&mut self, span: Span, message: String) {
        let err = EmitError { span, message };
        if !self.errors.contains(&err) {
            self.errors.push(err);
        }
    }

    pub(crate) fn state_width(&self, def: DefId) -> Option<Ty> {
        self.types.state_tys.get(&def).cloned()
    }

    /// A struct's flat field list: `(path, width)` per leaf `bits[N]`
    /// field — `path` is the field-name chain from the struct's own top
    /// level down to that leaf (`["inner", "a"]`, not just `"a"`) —
    /// recurses through any `Ty::Struct` field, so an arbitrarily nested
    /// struct flattens to the same shape a single-level one already did.
    /// Callers join `path` with `_` for the flat register/port name
    /// (`struct_lit_field_const` below takes the same `path` shape
    /// unjoined, to walk back into a nested struct literal). `check_
    /// struct_cycles` (types.rs) guarantees this recursion terminates.
    pub(crate) fn struct_field_widths(&self, struct_def: DefId) -> Option<Vec<(Vec<String>, u64)>> {
        let fields = self.types.struct_fields.get(&struct_def)?;
        let mut out = Vec::with_capacity(fields.len());
        for (name, ty) in fields {
            match ty {
                Ty::Bits(Width::Known(w)) => out.push((vec![name.clone()], *w)),
                Ty::Struct { def, .. } => {
                    for (mut sub_path, w) in self.struct_field_widths(*def)? {
                        sub_path.insert(0, name.clone());
                        out.push((sub_path, w));
                    }
                }
                Ty::Option(inner) => {
                    for (mut sub_path, w) in self.option_field_widths(inner)? {
                        sub_path.insert(0, name.clone());
                        out.push((sub_path, w));
                    }
                }
                _ => return None,
            }
        }
        Some(out)
    }

    /// `?T`'s flat field list — the same `(path, width)` shape `struct_
    /// field_widths` produces, over the compiler-synthesized `{ valid:
    /// bit, data: T }` shape `Ty::Option` sugars over. `T` itself may be
    /// another struct (or another `?T`, `??T` — untested but the same
    /// recursion that already generalizes `struct_field_widths` applies
    /// here too) via the `data` field's own recursion; a plain `bits[N]`
    /// `T` is the common case (`?bits[8]`).
    pub(crate) fn option_field_widths(&self, inner: &Ty) -> Option<Vec<(Vec<String>, u64)>> {
        let mut out = vec![(vec!["valid".to_string()], 1)];
        match inner {
            Ty::Bits(Width::Known(w)) => out.push((vec!["data".to_string()], *w)),
            Ty::Struct { def, .. } => {
                for (mut sub_path, w) in self.struct_field_widths(*def)? {
                    sub_path.insert(0, "data".to_string());
                    out.push((sub_path, w));
                }
            }
            Ty::Option(t) => {
                for (mut sub_path, w) in self.option_field_widths(t)? {
                    sub_path.insert(0, "data".to_string());
                    out.push((sub_path, w));
                }
            }
            _ => return None,
        }
        Some(out)
    }

    pub(crate) fn state_mem_ty(&self, def: DefId) -> Option<Ty> {
        self.types.state_tys.get(&def).cloned()
    }

    pub(crate) fn const_eval(&self, id: ExprId) -> Option<u64> {
        match self.ast.expr(id) {
            Expr::Int(v) => Some(*v),
            Expr::SizedInt { value, .. } => Some(*value),
            _ => None,
        }
    }

    /// A struct-typed reg/output's init (`= Pair{valid: 0, data: 0}`),
    /// one flat field at a time — `init` must literally be a struct
    /// literal (types.rs already required this: a struct-typed reg/
    /// output's declared type only unifies against a `StructLit`'s own
    /// inferred type). `path` walks into nested struct/`?T` literals one
    /// segment at a time (`["inner", "a"]` for a nested field), same
    /// join convention `struct_field_widths` uses for the flat name —
    /// `def` is `head`'s OWN struct's `DefId`, needed to look up each
    /// intermediate field's declared type before recursing: a nested
    /// STRUCT field's value is another real `Expr::StructLit` to keep
    /// walking structurally, but a nested `?T` field's value is
    /// `Expr::Absent`/a bare coerced value, which must hand off to
    /// `option_lit_field_const` instead of assuming `StructLit` all the
    /// way down (mirrors `writes.rs`'s `compile_field_path_value`,
    /// caught the same way — advisor flagged this fn's un-threaded
    /// `def` as the const-eval-time twin of that value-compile-time
    /// bug).
    pub(crate) fn struct_lit_field_const(
        &self,
        init: ExprId,
        path: &[String],
        def: DefId,
    ) -> Option<u64> {
        let Expr::StructLit { fields, .. } = self.ast.expr(init) else {
            return None;
        };
        let (head, rest) = path.split_first()?;
        let value = fields.iter().find(|(f, _)| f == head)?.1;
        if rest.is_empty() {
            self.const_eval(value)
        } else {
            let field_ty = self
                .types
                .struct_fields
                .get(&def)
                .and_then(|fs| fs.iter().find(|(n, _)| n == head))
                .map(|(_, t)| t.clone())?;
            match field_ty {
                Ty::Struct { def: sub_def, .. } => {
                    self.struct_lit_field_const(value, rest, sub_def)
                }
                Ty::Option(inner) => self.option_lit_field_const(value, rest, &inner),
                _ => None,
            }
        }
    }

    /// `?T`'s init, one flat field at a time — same role `struct_lit_
    /// field_const` has, over `false`/a coerced-present value instead
    /// of a real struct literal (`?T` has no literal AST form of its
    /// own): `valid` is `0`/`1` depending on whether `init` is literally
    /// `false`; `data` is `init` itself (there is no separate wrapper
    /// syntax to peel off a present value — the coerced expression IS
    /// the `T` value), or `0` when absent (a don't-care default). `data`'s
    /// own `rest` dispatches on `inner` (T itself struct- or `?`-shaped)
    /// rather than assuming struct, so a nested `??T` recurses correctly
    /// too.
    pub(crate) fn option_lit_field_const(
        &self,
        init: ExprId,
        path: &[String],
        inner: &Ty,
    ) -> Option<u64> {
        let (head, rest) = path.split_first()?;
        // `optional <sub>` forces THIS layer's `valid` to 1 regardless of
        // `sub`'s own shape, then peels to `sub` for `data` — unlike the
        // coerced-present case below, where presence never adds a layer
        // to peel off, `optional` is the one construction that DOES: it
        // is a real recursive-descent step, letting `sub` be itself
        // absent/another `optional`/a plain value, i.e. exactly the
        // `??T`/`Some(None)` construction the coerced-present case can't
        // express (both its layers are always equal).
        if let Expr::Optional(sub) = self.ast.expr(init) {
            let sub = *sub;
            return match head.as_str() {
                "valid" => Some(1),
                "data" if rest.is_empty() => self.const_eval(sub),
                "data" => match inner {
                    Ty::Struct { def, .. } => self.struct_lit_field_const(sub, rest, *def),
                    Ty::Option(t) => self.option_lit_field_const(sub, rest, t),
                    _ => None,
                },
                _ => None,
            };
        }
        let is_absent = matches!(self.ast.expr(init), Expr::Absent);
        match (head.as_str(), is_absent) {
            ("valid", absent) => Some(u64::from(!absent)),
            ("data", true) => Some(0),
            ("data", false) if rest.is_empty() => self.const_eval(init),
            ("data", false) => match inner {
                Ty::Struct { def, .. } => self.struct_lit_field_const(init, rest, *def),
                Ty::Option(t) => self.option_lit_field_const(init, rest, t),
                _ => None,
            },
            _ => None,
        }
    }
}
