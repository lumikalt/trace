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

use crate::ast::{Ast, Expr, ExprId, Item, ItemId, StmtId};
use crate::effects::Effects;
use crate::lexer::Span;
use crate::resolve::{DefId, Resolution};
use crate::schedule::Schedule;
use crate::types::{Ty, Types};
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
}
