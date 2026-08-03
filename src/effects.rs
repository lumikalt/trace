//! Effect checking: enforces the hardware "colors" from DESIGN.md.
//!
//! Declared effects come from `<...>` lists. Two of them are inferred as
//! well: `fails` (fallibility) and the `reads`/`writes` rows. The policy
//! is the same for all inferred effects: the compiler computes them; a
//! stated effect is an interface assertion. Overstating is allowed
//! (conservative), understating a row is an error.
//!
//! Construct rules enforced here:
//! - `tick`, `spawn`, `sync`, `race` require `<sequences>`.
//! - `while` requires `<sequences>` (one iteration per cycle) or
//!   `<elaborates>` (elaboration-time bound) — DESIGN.md's E012.
//! - `any` requires `<chooses>`; `chooses` is legal only on specs.
//! - Specs are verification-only: calling one from synthesizable code is
//!   an error.
//! - A call requires the callee's color: calling `<sequences>` code needs
//!   `<sequences>`. Calling `<elaborates>` code needs no particular
//!   caller color — any call site can name one, since `elaborate.rs`
//!   reduces every such call to a plain value in its own pre-pass, before
//!   this checker ever runs on the result.
//! - Recursion (direct self-call) requires `<elaborates>`.
//! - Elaboration positions (state types, initializers, `<elaborates>`
//!   bodies) are not failure contexts: guards are errors there.
//! - Rules are always failure contexts; `fails` needs no declaration.

use crate::ast::{Ast, Effect, Expr, ExprId, FnKind, Item, ItemId, Stmt, StmtId};
use crate::lexer::Span;
use crate::resolve::{DefId, DefKind, Resolution, is_guard_like};
use std::collections::{BTreeSet, HashMap};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EffectSig {
    pub combines: bool,
    pub sequences: bool,
    pub elaborates: bool,
    pub chooses: bool,
    /// Can fail. Declared or inferred (guards, fifo ops, failing calls).
    pub fails: bool,
    /// State this item reads, inferred, including through calls.
    pub reads: BTreeSet<DefId>,
    /// State this item writes, inferred, including through calls.
    pub writes: BTreeSet<DefId>,
}

#[derive(Debug, Default)]
pub struct Effects {
    /// Signature of every rule, fn, spec, and impl.
    pub sigs: HashMap<ItemId, EffectSig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectError {
    pub span: Span,
    pub message: String,
}

pub fn check(ast: &Ast, res: &Resolution) -> (Effects, Vec<EffectError>) {
    let mut checker = Checker {
        ast,
        res,
        def_items: res.item_defs.iter().map(|(i, d)| (*d, *i)).collect(),
        sigs: HashMap::new(),
        declared_rows: HashMap::new(),
        errors: Vec::new(),
    };
    let bodied = checker.collect_bodied_items();
    for id in &bodied {
        checker.declare_sig(*id);
    }
    checker.infer_fixpoint(&bodied);
    for id in &bodied {
        checker.check_item(*id);
    }
    checker.check_elab_positions();
    (Effects { sigs: checker.sigs }, checker.errors)
}

struct Checker<'a> {
    ast: &'a Ast,
    res: &'a Resolution,
    def_items: HashMap<DefId, ItemId>,
    sigs: HashMap<ItemId, EffectSig>,
    /// Stated `reads`/`writes` arg names per item, for understatement checks.
    declared_rows: HashMap<ItemId, (BTreeSet<String>, BTreeSet<String>)>,
    errors: Vec<EffectError>,
}

const COLOR_EFFECTS: &[&str] = &["combines", "sequences", "elaborates", "chooses", "fails"];

/// A `rule` is always a failure context (`fails` needs no declaration —
/// matching Verse's implicit top-level transaction). A `fn`/`impl`/
/// `spec` is one only if it explicitly declares `<fails>` in its own
/// `<...>` list — a SYNTACTIC check read straight off the AST, not
/// inferred from the body: inferring it would let a bare condition's
/// own presence bootstrap failure-context status for itself (and any
/// sibling bare condition), the self-reference an author-declared gate
/// (Verse's `<decides>`) exists specifically to avoid. `check_fails_
/// declared` (below) uses this to reject a computed-`fails` item that
/// never declared it — matching Verse's own "unhandled failure" error
/// rather than silently letting it compile, and this codebase's own
/// standing preference for an explicit error over a silent no-op.
fn is_failure_context(ast: &Ast, item: ItemId) -> bool {
    match ast.item(item) {
        Item::Rule { .. } => true,
        Item::Fn { effects, .. } => effects.iter().any(|e| e.name.text == "fails"),
        _ => false,
    }
}

impl<'a> Checker<'a> {
    fn error(&mut self, span: Span, message: String) {
        self.errors.push(EffectError { span, message });
    }

    /// All rule/fn items, recursively through modules.
    fn collect_bodied_items(&self) -> Vec<ItemId> {
        let mut out = Vec::new();
        let mut stack: Vec<ItemId> = self.ast.roots.clone();
        while let Some(id) = stack.pop() {
            match self.ast.item(id) {
                Item::Module { items, .. } => stack.extend(items.iter().copied()),
                Item::Rule { .. } | Item::Fn { .. } => out.push(id),
                _ => {}
            }
        }
        out
    }

    fn item_effects(&self, id: ItemId) -> &[Effect] {
        match self.ast.item(id) {
            Item::Rule { effects, .. } | Item::Fn { effects, .. } => effects,
            _ => &[],
        }
    }

    fn item_body(&self, id: ItemId) -> &[StmtId] {
        match self.ast.item(id) {
            Item::Rule { body, .. } | Item::Fn { body, .. } => body,
            _ => &[],
        }
    }

    fn is_spec(&self, id: ItemId) -> bool {
        matches!(
            self.ast.item(id),
            Item::Fn {
                kind: FnKind::Spec,
                ..
            }
        )
    }

    /// Parse the declared `<...>` list into a signature, with validation.
    fn declare_sig(&mut self, id: ItemId) {
        let mut sig = EffectSig::default();
        let mut declared_reads = BTreeSet::new();
        let mut declared_writes = BTreeSet::new();
        for effect in self.item_effects(id).to_vec() {
            let name = effect.name.text.as_str();
            match name {
                "combines" => sig.combines = true,
                "sequences" => sig.sequences = true,
                "elaborates" => sig.elaborates = true,
                "chooses" => sig.chooses = true,
                "fails" => sig.fails = true,
                "reads" | "writes" => {
                    let target = if name == "reads" {
                        &mut declared_reads
                    } else {
                        &mut declared_writes
                    };
                    for arg in &effect.args {
                        target.insert(arg.text.clone());
                    }
                    continue;
                }
                _ => {
                    self.error(
                        effect.name.span.clone(),
                        format!(
                            "unknown effect `{name}` (expected combines, sequences, \
                             elaborates, chooses, fails, reads, or writes)"
                        ),
                    );
                    continue;
                }
            }
            if COLOR_EFFECTS.contains(&name) && !effect.args.is_empty() {
                self.error(
                    effect.name.span.clone(),
                    format!("`{name}` takes no arguments"),
                );
            }
        }
        let span = self.ast.item_spans[id.0 as usize].clone();
        if sig.combines && sig.sequences {
            self.error(
                span.clone(),
                "`combines` (combinational) contradicts `sequences` (sequential)".to_string(),
            );
        }
        if sig.combines && sig.elaborates {
            self.error(
                span.clone(),
                "`combines` (circuit) contradicts `elaborates` (elaboration-time)".to_string(),
            );
        }
        if sig.sequences && sig.elaborates {
            self.error(
                span.clone(),
                "`elaborates` code runs at elaboration time and cannot span multiple cycles"
                    .to_string(),
            );
        }
        if sig.chooses && !self.is_spec(id) {
            self.error(
                span,
                "`chooses` marks verification-only code; only a `spec` may declare it".to_string(),
            );
        }
        self.declared_rows
            .insert(id, (declared_reads, declared_writes));
        self.sigs.insert(id, sig);
    }

    /// Iterate `fails` and row inference to a fixed point: effects flow
    /// from callee to caller, and the call graph may be in any order.
    fn infer_fixpoint(&mut self, bodied: &[ItemId]) {
        for _ in 0..=bodied.len() {
            let mut changed = false;
            for id in bodied {
                let mut sig = self.sigs[id].clone();
                for stmt in self.item_body(*id).to_vec() {
                    self.infer_stmt(stmt, &mut sig);
                }
                if sig != self.sigs[id] {
                    self.sigs.insert(*id, sig);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }

    fn infer_stmt(&self, id: StmtId, sig: &mut EffectSig) {
        match self.ast.stmt(id) {
            Stmt::Expr(e) => {
                // A bare expression whose value is left unused — not a
                // call, fifo op, or spawn, each of which already has
                // its own independent bare-statement meaning —
                // implicitly gates this rule, the same as an explicit
                // `e?` would (whose own `Expr::Guard` arm below also
                // sets this, redundantly but harmlessly).
                if is_guard_like(self.ast, self.res, *e) {
                    sig.fails = true;
                }
                self.infer_expr(*e, sig);
            }
            Stmt::Assign { lhs, rhs } => {
                self.infer_expr(*rhs, sig);
                self.infer_write(*lhs, sig);
            }
            Stmt::Let { init, .. } => self.infer_expr(*init, sig),
            Stmt::Tick => {}
            Stmt::Return(e) => {
                if let Some(e) = e {
                    self.infer_expr(*e, sig);
                }
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                // A BARE comparison directly as an `if`'s own condition is
                // branch-scoped and discharged right here (types.rs's
                // `check_cond` is what actually allows this shape) --
                // mirrors `Expr::Logic`'s arm below: merge the operands'
                // `reads`, but not the ordinary `Expr::Binary` comparison
                // arm's `fails = true`, or every such `if` would need
                // `<fails>` declared for a condition that never gates
                // anything (see DESIGN.md's "`if`: branch-scoped fallible
                // conditions").
                if let Expr::Binary { op, lhs, rhs } = self.ast.expr(*cond)
                    && op.is_comparison()
                {
                    self.infer_expr(*lhs, sig);
                    self.infer_expr(*rhs, sig);
                } else if let Expr::Bracket { callee, args } = self.ast.expr(*cond)
                    && let Some(fifo) = self.fifo_op_target(*callee)
                {
                    // `if fifo.Deq[] { ... }`: same reasoning as the
                    // comparison case just above, mirrored from `Stmt::
                    // IfLet`'s own identical branch below -- reads+writes
                    // the fifo but does NOT set `sig.fails`.
                    sig.reads.insert(fifo);
                    sig.writes.insert(fifo);
                    for arg in args {
                        self.infer_expr(*arg, sig);
                    }
                } else if let Expr::Call { callee, args } = self.ast.expr(*cond) {
                    // `if Classify(a) { ... }`: same reasoning, mirrored
                    // from `Stmt::IfLet`'s own identical branch below.
                    if let Some(callee_sig) = self.callee_sig(*callee) {
                        sig.reads.extend(callee_sig.reads.iter().copied());
                        sig.writes.extend(callee_sig.writes.iter().copied());
                    }
                    self.infer_expr(*callee, sig);
                    for arg in args {
                        self.infer_expr(*arg, sig);
                    }
                } else {
                    self.infer_expr(*cond, sig);
                }
                for s in then_body {
                    self.infer_stmt(*s, sig);
                }
                if let Some(else_body) = else_body {
                    for s in else_body {
                        self.infer_stmt(*s, sig);
                    }
                }
            }
            Stmt::IfLet {
                init,
                then_body,
                else_body,
                ..
            } => {
                // `if let x = opt?`'s own unwrap is branch-scoped and
                // discharged right here -- mirrors `Stmt::If`'s
                // comparison-discharge arm above: `init` is `Expr::Guard
                // (inner)` (types.rs's own arm requires this shape), so
                // merge `inner`'s reads directly rather than routing
                // through `Expr::Guard`'s ordinary arm below, which would
                // set `sig.fails = true` -- wrong here, since this
                // presence check never gates the enclosing item the way
                // an explicit top-level `opt?` does.
                if let Expr::Guard(inner) = self.ast.expr(*init) {
                    self.infer_expr(*inner, sig);
                } else if let Expr::Bracket { callee, args } = self.ast.expr(*init)
                    && let Some(fifo) = self.fifo_op_target(*callee)
                {
                    // `if let x = fifo.Deq[] { ... }`: reads+writes the
                    // fifo (a real dequeue happens whenever `then` is
                    // taken) but, like the Option-presence case just
                    // above, does NOT set `sig.fails` -- this presence
                    // check never gates the enclosing rule/fn, unlike an
                    // ordinary top-level `Deq[]`'s own `Expr::Bracket` arm
                    // (below) does for every OTHER position.
                    sig.reads.insert(fifo);
                    sig.writes.insert(fifo);
                    for arg in args {
                        self.infer_expr(*arg, sig);
                    }
                } else if let Expr::Call { callee, args } = self.ast.expr(*init) {
                    // `if let x = Classify(a) { ... }`: reads+writes from
                    // the callee's own effect signature (a real call
                    // happens whenever `then` is taken), same reasoning
                    // as the fifo-Deq case above -- does NOT set `sig.
                    // fails`, unlike the ordinary `Expr::Call` arm below.
                    if let Some(callee_sig) = self.callee_sig(*callee) {
                        sig.reads.extend(callee_sig.reads.iter().copied());
                        sig.writes.extend(callee_sig.writes.iter().copied());
                    }
                    self.infer_expr(*callee, sig);
                    for arg in args {
                        self.infer_expr(*arg, sig);
                    }
                } else {
                    self.infer_expr(*init, sig);
                }
                for s in then_body {
                    self.infer_stmt(*s, sig);
                }
                if let Some(else_body) = else_body {
                    for s in else_body {
                        self.infer_stmt(*s, sig);
                    }
                }
            }
            Stmt::While { cond, body } => {
                self.infer_expr(*cond, sig);
                for s in body {
                    self.infer_stmt(*s, sig);
                }
            }
            Stmt::WhileLet { init, body, .. } => {
                // Mirrors `Stmt::IfLet`'s own arm above exactly, for the
                // identical reason: `init` is `Expr::Guard(inner)`
                // (types.rs's own arm requires this), and the presence
                // check never gates the enclosing item the way a bare
                // top-level `opt?` does.
                if let Expr::Guard(inner) = self.ast.expr(*init) {
                    self.infer_expr(*inner, sig);
                } else {
                    self.infer_expr(*init, sig);
                }
                for s in body {
                    self.infer_stmt(*s, sig);
                }
            }
        }
    }

    /// An assignment target: the root state def is written, indices read.
    fn infer_write(&self, id: ExprId, sig: &mut EffectSig) {
        match self.ast.expr(id) {
            Expr::Ident(_) => {
                if let Some(def) = self.state_def(id) {
                    sig.writes.insert(def);
                }
            }
            Expr::Bracket { callee, args } => {
                if let Some(def) = self.state_def(*callee) {
                    sig.writes.insert(def);
                } else {
                    self.infer_expr(*callee, sig);
                }
                for arg in args {
                    self.infer_expr(*arg, sig);
                }
            }
            // `inst.port := v` — writes that one port's own resource
            // (resolve.rs synthesizes one per `(inst, port)` pair), not
            // the whole instance: two rules writing different ports of
            // the same instance don't conflict.
            Expr::Field { base, .. } => {
                if let Some(def) = self.state_def(id) {
                    sig.writes.insert(def);
                } else {
                    self.infer_expr(*base, sig);
                }
            }
            _ => self.infer_expr(id, sig),
        }
    }

    fn infer_expr(&self, id: ExprId, sig: &mut EffectSig) {
        match self.ast.expr(id) {
            Expr::Ident(_) => {
                if let Some(def) = self.state_def(id) {
                    sig.reads.insert(def);
                }
            }
            Expr::Int(_) | Expr::SizedInt { .. } | Expr::Wildcard => {}
            Expr::Unary { operand, .. } => self.infer_expr(*operand, sig),
            // A comparison fails wherever it appears (like a fifo op or
            // failing call, and unlike every other binop) — conservative
            // by construction, matching those two: correctness doesn't
            // depend on WHERE it's nested, only on not missing one.
            Expr::Binary { op, lhs, rhs } => {
                if op.is_comparison() {
                    sig.fails = true;
                }
                self.infer_expr(*lhs, sig);
                self.infer_expr(*rhs, sig);
            }
            Expr::Guard(inner) => {
                sig.fails = true;
                self.infer_expr(*inner, sig);
            }
            // `inst.port` read — that port's own resource (see the write
            // arm in `infer_write` for why this isn't the whole instance).
            Expr::Field { base, .. } => {
                if let Some(def) = self.state_def(id) {
                    sig.reads.insert(def);
                } else {
                    self.infer_expr(*base, sig);
                }
            }
            Expr::Bracket { callee, args } => {
                // A fifo op (`f.Deq[]` / `f.Enq[x]`) can fail and mutates
                // the fifo: reads + writes + fails, conservatively.
                if let Some(fifo) = self.fifo_op_target(*callee) {
                    sig.fails = true;
                    sig.reads.insert(fifo);
                    sig.writes.insert(fifo);
                } else {
                    self.infer_expr(*callee, sig);
                }
                for arg in args {
                    self.infer_expr(*arg, sig);
                }
            }
            Expr::Call { callee, args } => {
                if let Some(callee_sig) = self.callee_sig(*callee) {
                    sig.fails |= callee_sig.fails;
                    sig.reads.extend(callee_sig.reads.iter().copied());
                    sig.writes.extend(callee_sig.writes.iter().copied());
                }
                self.infer_expr(*callee, sig);
                for arg in args {
                    self.infer_expr(*arg, sig);
                }
            }
            Expr::Spawn(inner) => self.infer_expr(*inner, sig),
            Expr::ListLit(items) => {
                for item in items {
                    self.infer_expr(*item, sig);
                }
            }
            // A list-slice bound (`xs[..mid]`) is elaboration-time only —
            // `mid`/etc reference elaboration-time locals inside an
            // `<elaborates>` body, never a circuit state def, but walk
            // them anyway for uniformity (harmless no-op if so).
            Expr::Range { lo, hi } => {
                if let Some(lo) = lo {
                    self.infer_expr(*lo, sig);
                }
                if let Some(hi) = hi {
                    self.infer_expr(*hi, sig);
                }
            }
            // `A or B or C` — unlike `logic`, a selected alternative's
            // fifo op is a REAL effect (mutates the fifo), so its
            // reads/writes are NOT discharged. Only `fails` is special:
            // each alternative failing individually doesn't fail the
            // whole chain, only ALL of them failing does — and if the
            // last alternative isn't itself a fifo op (a plain default
            // value instead), the chain can never fail at all. v0 shape
            // validation (every alt but possibly the last must be a fifo
            // op) lives in firrtl/checks.rs; this stays permissive and
            // conservative like the rest of `infer_expr`.
            Expr::Or(alts) => {
                let last = alts.len().saturating_sub(1);
                for (i, alt) in alts.iter().enumerate() {
                    if let Expr::Bracket { callee, args } = self.ast.expr(*alt)
                        && let Some(fifo) = self.fifo_op_target(*callee)
                    {
                        sig.reads.insert(fifo);
                        sig.writes.insert(fifo);
                        for arg in args {
                            self.infer_expr(*arg, sig);
                        }
                        if i == last {
                            sig.fails = true;
                        }
                    } else {
                        self.infer_expr(*alt, sig);
                    }
                }
            }
            Expr::StructLit { fields, base, .. } => {
                for (_, value) in fields {
                    self.infer_expr(*value, sig);
                }
                // `..base` reads every field `fields` doesn't name off
                // `base` -- omitting this walk would silently drop
                // `base` from `sig.reads`, so `p := Pair{ x: 1, ..p }`
                // wouldn't register as reading `p` at all, and the
                // scheduler could then let another rule write `p`
                // concurrently: wrong hardware, not just a missing
                // feature, and invisible without simulating.
                if let Some(base) = base {
                    self.infer_expr(*base, sig);
                }
            }
            // Type-position only (`?T`); never appears in a rule/fn
            // body's own value expressions.
            Expr::OptionTy(_) => {}
            Expr::Absent => {}
            Expr::Optional(inner) => self.infer_expr(*inner, sig),
            // `logic <expr>` DISCHARGES `expr`'s failure into a plain
            // boolean value: neither `fails` nor `writes` propagate to
            // the enclosing sig (the whole point — see TODO.md's
            // `logic` design note). Compute `expr`'s own effect in
            // ISOLATION and merge only its `reads` back in, still
            // needed so scheduling correctly orders this rule against
            // anything that writes the same resource being probed.
            // `firrtl/checks.rs`'s `check_logic_args` separately
            // rejects a shape this isolation would otherwise silently
            // accept but that isn't actually meaningful (not a fifo
            // op/failing call at all, or a callee that also writes
            // state).
            //
            // A `logic`-wrapped CALL is the one shape where full
            // isolation is wrong: `logic Check(a > b)` only discharges
            // `Check`'s OWN fail condition, not an independent
            // comparison (or anything else) inside its ARGUMENTS —
            // those are ordinary caller-side expressions evaluated
            // before the call, not part of what `logic` is discharging.
            // Advisor-caught before commit: the isolated-inner_sig path
            // used to swallow `a > b`'s own `fails` here, so a wrapping
            // fn could get away with declaring `<combines>` (no
            // `<fails>`) despite `e97795e`'s "must declare `<fails>`
            // wherever it's computed true" rule, and the guard was
            // silently dropped at the fn boundary. Fixed by handling
            // the call shape exactly like an ordinary `Expr::Call`
            // (callee reads merged, args inferred straight into `sig`)
            // but WITHOUT merging the callee's own `fails`/`writes` —
            // that part alone is what `logic` discharges.
            //
            // A `logic`-wrapped FIFO OP is the identical shape, one
            // level later (found live, not by inspection: `Wrap() :
            // [1] <combines> { return logic f.Enq[a > b] }` used to
            // compile clean with no error, `a > b`'s own `fails` folded
            // into and lost inside the SAME `inner_sig.fails` boolean
            // the occupancy test itself sets — `Expr::Bracket`'s own
            // arm above merges an Enq/Deq's ARGUMENT effects into
            // whatever `sig` it's given, so isolating the whole bracket
            // expr conflates "does the occupancy test fail" with "does
            // the argument itself fail," indistinguishable once both
            // are the same flag). `logic f.Enq[x]`/`logic f.Deq[]`
            // themselves stay a pure occupancy TEST — `writes` still
            // excluded (`compile_logic` never emits the actual
            // mutation, existing intentional behavior, not touched
            // here) — but the argument, like a call's arguments, is an
            // ordinary caller-side expression `logic` never reaches:
            // inferred straight into `sig`, not isolated, so an
            // embedded `a > b` propagates its own `fails`/`reads`
            // correctly. `fifo_op_target`'s own `reads.insert` covers
            // the occupancy resource itself, same as the plain
            // (non-`logic`) fifo-op arm above.
            Expr::Logic(inner) => {
                if let Expr::Call { callee, args } = self.ast.expr(*inner).clone() {
                    if let Some(callee_sig) = self.callee_sig(callee) {
                        sig.reads.extend(callee_sig.reads.iter().copied());
                    }
                    self.infer_expr(callee, sig);
                    for arg in args {
                        self.infer_expr(arg, sig);
                    }
                } else if let Expr::Bracket { callee, args } = self.ast.expr(*inner).clone()
                    && let Some(fifo) = self.fifo_op_target(callee)
                {
                    sig.reads.insert(fifo);
                    for arg in args {
                        self.infer_expr(arg, sig);
                    }
                } else {
                    let mut inner_sig = EffectSig::default();
                    self.infer_expr(*inner, &mut inner_sig);
                    sig.reads.extend(inner_sig.reads);
                }
            }
        }
    }

    /// The state def behind an ident expression, if any.
    fn state_def(&self, id: ExprId) -> Option<DefId> {
        let def = self.res.expr_defs.get(&id)?;
        self.res.def(*def).kind.is_state().then_some(*def)
    }

    /// `callee` of a Bracket that is `fifo.Something` -> the fifo def.
    fn fifo_op_target(&self, callee: ExprId) -> Option<DefId> {
        let Expr::Field { base, .. } = self.ast.expr(callee) else {
            return None;
        };
        let def = self.res.expr_defs.get(base)?;
        (self.res.def(*def).kind == DefKind::Fifo).then_some(*def)
    }

    /// Signature of a called fn/impl, if the callee resolves to one.
    fn callee_sig(&self, callee: ExprId) -> Option<&EffectSig> {
        let def = self.res.expr_defs.get(&callee)?;
        let item = self.def_items.get(def)?;
        self.sigs.get(item)
    }

    // --- error pass ---

    fn check_item(&mut self, id: ItemId) {
        let sig = self.sigs[&id].clone();
        // Elaboration bodies are not failure contexts.
        let elab = sig.elaborates;
        for stmt in self.item_body(id).to_vec() {
            self.check_stmt(stmt, id, &sig, elab);
        }
        self.check_rows(id, &sig);
        self.check_fails_declared(id, &sig);
    }

    fn check_stmt(&mut self, id: StmtId, item: ItemId, sig: &EffectSig, elab: bool) {
        let span = self.ast.stmt_spans[id.0 as usize].clone();
        match self.ast.stmt(id).clone() {
            Stmt::Tick => {
                if !sig.sequences {
                    self.error(
                        span,
                        "`tick` requires `<sequences>` on the enclosing item".to_string(),
                    );
                }
            }
            Stmt::While { cond, body } => {
                if !sig.sequences && !sig.elaborates {
                    self.error(
                        span,
                        "loop needs `<sequences>` (one iteration per cycle) or an \
                         elaboration-time bound under `<elaborates>`"
                            .to_string(),
                    );
                }
                self.check_expr(cond, item, sig, elab);
                for s in body {
                    self.check_stmt(s, item, sig, elab);
                }
            }
            Stmt::Expr(e) => {
                // Explicit `e?` gets this same error from `Expr::Guard`'s
                // own arm below (via the recursive `check_expr` call);
                // excluded here so an elaboration-time guard doesn't get
                // reported twice. Whether this (or an explicit `?`) is
                // legal at all in this item without a `<fails>`
                // declaration is checked once, item-wide, by `check_
                // fails_declared` — not here per-statement (see its own
                // doc comment for why one check covering all three
                // fails-inducing constructs uniformly, mirroring `check_
                // rows`, is better than a scattered per-site version of
                // the same question).
                if elab
                    && is_guard_like(self.ast, self.res, e)
                    && !matches!(self.ast.expr(e), Expr::Guard(_))
                {
                    self.error(
                        span,
                        "an unused value here implicitly guards the enclosing scope \
                         (the same thing writing `?` would do), which cannot fail at \
                         elaboration time (no transaction to abort)"
                            .to_string(),
                    );
                }
                self.check_expr(e, item, sig, elab)
            }
            Stmt::Assign { lhs, rhs } => {
                self.check_expr(lhs, item, sig, elab);
                self.check_expr(rhs, item, sig, elab);
            }
            Stmt::Let { init, .. } => self.check_expr(init, item, sig, elab),
            Stmt::Return(e) => {
                // A `rule` has no return value to give back to anything —
                // `return` only makes sense inside a `fn`/`spec`/`impl`
                // body (all `Item::Fn`). Left unchecked, this used to
                // parse and even emit "successfully": nothing downstream
                // (guard placement, read-site collection, write-in-stmts)
                // has a case for `Stmt::Return` at a rule's top level, so
                // the whole statement — and anything it does, like a fifo
                // `Deq`/guard — silently vanished with zero error.
                if !matches!(self.ast.item(item), Item::Fn { .. }) {
                    self.error(
                        span,
                        "`return` is only valid inside a function body; a `rule` has no \
                         return value (this used to compile and silently drop the whole \
                         statement — if this was meant to gate the rule, write it as an \
                         ordinary guard or fifo operation instead)"
                            .to_string(),
                    );
                }
                if let Some(e) = e {
                    self.check_expr(e, item, sig, elab);
                }
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                self.check_expr(cond, item, sig, elab);
                for s in then_body {
                    self.check_stmt(s, item, sig, elab);
                }
                for s in else_body.unwrap_or_default() {
                    self.check_stmt(s, item, sig, elab);
                }
            }
            Stmt::IfLet {
                init,
                then_body,
                else_body,
                ..
            } => {
                // `init` is `Expr::Guard(inner)` (types.rs requires this
                // shape) -- `check_expr`'s own `Expr::Guard` arm already
                // errors "guards cannot fail at elaboration time" when
                // `elab` is true, the identical rejection an ordinary
                // `opt?` gets there; no separate elaborates-specific
                // check needed here, this falls out of the existing
                // recursive call for free.
                self.check_expr(init, item, sig, elab);
                for s in then_body {
                    self.check_stmt(s, item, sig, elab);
                }
                for s in else_body.unwrap_or_default() {
                    self.check_stmt(s, item, sig, elab);
                }
            }
            Stmt::WhileLet { init, body, .. } => {
                // A loop, same as `Stmt::While`'s own arm above -- needs
                // the identical `<sequences>`/`<elaborates>` requirement
                // (`while let` is still "one iteration per cycle", just
                // with an Option-presence condition instead of a plain
                // comparison). `init`'s own guard-in-elaborates rejection
                // falls out of `check_expr` for free, same as `IfLet`'s
                // arm above.
                if !sig.sequences && !sig.elaborates {
                    self.error(
                        span,
                        "loop needs `<sequences>` (one iteration per cycle) or an \
                         elaboration-time bound under `<elaborates>`"
                            .to_string(),
                    );
                }
                self.check_expr(init, item, sig, elab);
                for s in body {
                    self.check_stmt(s, item, sig, elab);
                }
            }
        }
    }

    fn check_expr(&mut self, id: ExprId, item: ItemId, sig: &EffectSig, elab: bool) {
        let span = self.ast.expr_spans[id.0 as usize].clone();
        match self.ast.expr(id).clone() {
            Expr::Guard(inner) => {
                if elab {
                    self.error(
                        span,
                        "guards cannot fail at elaboration time (no transaction to abort)"
                            .to_string(),
                    );
                }
                self.check_expr(inner, item, sig, elab);
            }
            Expr::Spawn(inner) => {
                if !sig.sequences {
                    self.error(span.clone(), "`spawn` requires `<sequences>`".to_string());
                }
                match self.ast.expr(inner).clone() {
                    Expr::Call { callee, .. } => {
                        if let Some(def) = self.res.expr_defs.get(&callee).copied() {
                            let callee_def = self.res.def(def).clone();
                            if callee_def.kind != DefKind::Fn {
                                self.error(
                                    span.clone(),
                                    format!(
                                        "`spawn` can only start a `<sequences>` fn, not `{}`",
                                        callee_def.name
                                    ),
                                );
                            } else if let Some(target) = self.def_items.get(&def).copied()
                                && !self.sigs[&target].sequences
                            {
                                self.error(
                                    span.clone(),
                                    format!(
                                        "`spawn`'s callee `{}` must itself be declared \
                                         `<sequences>`",
                                        callee_def.name
                                    ),
                                );
                            }
                        }
                    }
                    _ => {
                        self.error(
                            span.clone(),
                            "`spawn` needs a direct function call, e.g. `spawn Foo(a, b)`"
                                .to_string(),
                        );
                    }
                }
                self.check_expr(inner, item, sig, elab);
            }
            Expr::Call { callee, args } => {
                self.check_call(callee, item, sig, span);
                for arg in args {
                    self.check_expr(arg, item, sig, elab);
                }
            }
            Expr::Bracket { callee, args } => {
                if elab && self.fifo_op_target(callee).is_some() {
                    self.error(
                        span.clone(),
                        "fifo operations cannot fail at elaboration time".to_string(),
                    );
                }
                // A builtin called via brackets (`sync[...]`, `race[...]`)
                // needs the same callee-name checks (e.g. `<sequences>`
                // required) that a paren call already gets — brackets are
                // a distinct AST shape from `Expr::Call`, so that check
                // doesn't run for free here.
                self.check_call(callee, item, sig, span);
                self.check_expr(callee, item, sig, elab);
                for arg in args {
                    self.check_expr(arg, item, sig, elab);
                }
            }
            Expr::Unary { operand, .. } => self.check_expr(operand, item, sig, elab),
            Expr::Optional(inner) | Expr::Logic(inner) => self.check_expr(inner, item, sig, elab),
            Expr::Binary { lhs, rhs, .. } => {
                self.check_expr(lhs, item, sig, elab);
                self.check_expr(rhs, item, sig, elab);
            }
            Expr::Field { base, .. } => self.check_expr(base, item, sig, elab),
            Expr::ListLit(items) => {
                for item_expr in items {
                    self.check_expr(item_expr, item, sig, elab);
                }
            }
            Expr::Range { lo, hi } => {
                if let Some(lo) = lo {
                    self.check_expr(lo, item, sig, elab);
                }
                if let Some(hi) = hi {
                    self.check_expr(hi, item, sig, elab);
                }
            }
            Expr::Or(alts) => {
                for alt in alts {
                    self.check_expr(alt, item, sig, elab);
                }
            }
            Expr::StructLit { fields, base, .. } => {
                for (_, value) in fields {
                    self.check_expr(value, item, sig, elab);
                }
                if let Some(base) = base {
                    self.check_expr(base, item, sig, elab);
                }
            }
            Expr::Ident(_)
            | Expr::Int(_)
            | Expr::SizedInt { .. }
            | Expr::Wildcard
            | Expr::OptionTy(_)
            | Expr::Absent => {}
        }
    }

    fn check_call(&mut self, callee: ExprId, item: ItemId, sig: &EffectSig, span: Span) {
        let Some(def) = self.res.expr_defs.get(&callee).copied() else {
            return;
        };
        let callee_def = self.res.def(def).clone();
        match callee_def.kind {
            DefKind::Builtin => match callee_def.name.as_str() {
                "any" => {
                    if !sig.chooses {
                        self.error(
                            span,
                            "`any` is nondeterministic choice and requires `<chooses>` \
                             (spec-only)"
                                .to_string(),
                        );
                    }
                }
                "sync" | "race" if !sig.sequences => {
                    self.error(
                        span,
                        format!("`{}` requires `<sequences>`", callee_def.name),
                    );
                }
                _ => {}
            },
            DefKind::Spec => {
                if !self.is_spec(item) {
                    self.error(
                        span,
                        format!(
                            "spec `{}` is verification-only and cannot be called from \
                             synthesizable code",
                            callee_def.name
                        ),
                    );
                }
            }
            DefKind::Fn | DefKind::Impl => {
                let Some(target) = self.def_items.get(&def).copied() else {
                    return;
                };
                if target == item && !sig.elaborates {
                    self.error(
                        span,
                        format!(
                            "recursive call to `{}` requires `<elaborates>` \
                             (elaboration-time recursion only)",
                            callee_def.name
                        ),
                    );
                    return;
                }
                let callee_sig = self.sigs[&target].clone();
                if callee_sig.sequences && !sig.sequences {
                    self.error(
                        span,
                        format!(
                            "calling `<sequences>` function `{}` requires `<sequences>`",
                            callee_def.name
                        ),
                    );
                }
            }
            _ => {}
        }
    }

    /// Stated rows must cover the inferred rows (overstating is allowed).
    fn check_rows(&mut self, id: ItemId, sig: &EffectSig) {
        let Some((declared_reads, declared_writes)) = self.declared_rows.get(&id).cloned() else {
            return;
        };
        let span = self.ast.item_spans[id.0 as usize].clone();
        for (declared, inferred, verb) in [
            (&declared_reads, &sig.reads, "reads"),
            (&declared_writes, &sig.writes, "writes"),
        ] {
            if declared.is_empty() {
                continue;
            }
            for def in inferred.iter() {
                let name = &self.res.def(*def).name;
                if !declared.contains(name) {
                    self.error(
                        span.clone(),
                        format!("item {verb} `{name}` but its `{verb}` row does not include it"),
                    );
                }
            }
        }
    }

    /// Unlike the rows check above (opt-in: only checked if the
    /// interface was stated at all), this one is mandatory: any
    /// `fn`/`impl`/`spec` whose computed `sig.fails` is true — because
    /// of a guard, a fifo operation, or a call to another failing
    /// item, DESIGN.md's own "fails" section lists all three together
    /// as equally fails-inducing — must itself declare `<fails>`,
    /// matching Verse's own `<decides>` requirement (a failure
    /// construct needs an author-declared context able to handle it,
    /// wherever it's reached from, not just where it's written).
    /// Skipped for `<elaborates>` items: those get their own, more
    /// specific "cannot fail at elaboration time" error straight from
    /// `check_expr`/`check_stmt`'s Guard-arm elab checks, so this
    /// mustn't ALSO fire and double-report the same body. Rules are
    /// exempt — `is_failure_context` already treats them as always
    /// failure contexts, needing no declaration.
    fn check_fails_declared(&mut self, id: ItemId, sig: &EffectSig) {
        if sig.elaborates || !sig.fails || is_failure_context(self.ast, id) {
            return;
        }
        let name = match self.ast.item(id) {
            Item::Rule { name, .. } | Item::Fn { name, .. } => name.text.clone(),
            _ => return,
        };
        let span = self.ast.item_spans[id.0 as usize].clone();
        self.error(
            span,
            format!(
                "`{name}` can fail (a guard, a fifo operation, or a call to failing \
                 code somewhere in its body) but does not declare `<fails>` — add it \
                 to this item's own effect list"
            ),
        );
    }

    /// State types and initializers are elaboration positions: no failure.
    fn check_elab_positions(&mut self) {
        let mut stack: Vec<ItemId> = self.ast.roots.clone();
        let empty = EffectSig {
            elaborates: true,
            ..EffectSig::default()
        };
        while let Some(id) = stack.pop() {
            match self.ast.item(id).clone() {
                Item::Module { items, .. } => stack.extend(items),
                Item::Reg { ty, init, .. } => {
                    self.check_expr(ty, id, &empty, true);
                    if let Some(init) = init {
                        self.check_expr(init, id, &empty, true);
                    }
                }
                Item::Mem { ty, .. } | Item::Fifo { ty, .. } | Item::Input { ty, .. } => {
                    self.check_expr(ty, id, &empty, true);
                }
                Item::Output { ty, init, .. } => {
                    self.check_expr(ty, id, &empty, true);
                    if let Some(init) = init {
                        self.check_expr(init, id, &empty, true);
                    }
                }
                Item::Inst { module, .. } => {
                    self.check_expr(module, id, &empty, true);
                }
                _ => {}
            }
        }
    }
}
