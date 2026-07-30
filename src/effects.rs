//! Effect checking: enforces the hardware "colors" from DESIGN.md.
//!
//! Declared effects come from `<...>` lists. Two of them are inferred as
//! well: `decides` (fallibility) and the `reads`/`writes` rows. The policy
//! is the same for all inferred effects: the compiler computes them; a
//! stated effect is an interface assertion. Overstating is allowed
//! (conservative), understating a row is an error.
//!
//! Construct rules enforced here:
//! - `tick`, `spawn`, `sync`, `race` require `<suspends>`.
//! - `while` requires `<suspends>` (one iteration per cycle) or
//!   `<allocates>` (elaboration-time bound) — DESIGN.md's E012.
//! - `any` requires `<choice>`; `choice` is legal only on specs.
//! - Specs are verification-only: calling one from synthesizable code is
//!   an error.
//! - A call requires the callee's color: calling `<suspends>` code needs
//!   `<suspends>`, calling `<allocates>` code needs `<allocates>`.
//! - Recursion (direct self-call) requires `<allocates>`.
//! - Elaboration positions (state types, initializers, `<allocates>`
//!   bodies) are not failure contexts: guards are errors there.
//! - Rules are always failure contexts; `decides` needs no declaration.

use crate::ast::{Ast, Effect, Expr, ExprId, FnKind, Item, ItemId, Stmt, StmtId};
use crate::lexer::Span;
use crate::resolve::{DefId, DefKind, Resolution};
use std::collections::{BTreeSet, HashMap};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EffectSig {
    pub converges: bool,
    pub suspends: bool,
    pub allocates: bool,
    pub choice: bool,
    /// Can fail. Declared or inferred (guards, fifo ops, deciding calls).
    pub decides: bool,
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

const COLOR_EFFECTS: &[&str] = &["converges", "suspends", "allocates", "choice", "decides"];

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
                "converges" => sig.converges = true,
                "suspends" => sig.suspends = true,
                "allocates" => sig.allocates = true,
                "choice" => sig.choice = true,
                "decides" => sig.decides = true,
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
                            "unknown effect `{name}` (expected converges, suspends, \
                             allocates, choice, decides, reads, or writes)"
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
        if sig.converges && sig.suspends {
            self.error(
                span.clone(),
                "`converges` (combinational) contradicts `suspends` (sequential)".to_string(),
            );
        }
        if sig.converges && sig.allocates {
            self.error(
                span.clone(),
                "`converges` (circuit) contradicts `allocates` (elaboration-time)".to_string(),
            );
        }
        if sig.suspends && sig.allocates {
            self.error(
                span.clone(),
                "`allocates` code runs at elaboration time and cannot suspend".to_string(),
            );
        }
        if sig.choice && !self.is_spec(id) {
            self.error(
                span,
                "`choice` marks verification-only code; only a `spec` may declare it".to_string(),
            );
        }
        self.declared_rows
            .insert(id, (declared_reads, declared_writes));
        self.sigs.insert(id, sig);
    }

    /// Iterate `decides` and row inference to a fixed point: effects flow
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
            Stmt::Expr(e) => self.infer_expr(*e, sig),
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
                self.infer_expr(*cond, sig);
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
            Expr::Int(_) | Expr::Wildcard => {}
            Expr::Unary { operand, .. } => self.infer_expr(*operand, sig),
            Expr::Binary { lhs, rhs, .. } => {
                self.infer_expr(*lhs, sig);
                self.infer_expr(*rhs, sig);
            }
            Expr::Guard(inner) => {
                sig.decides = true;
                self.infer_expr(*inner, sig);
            }
            Expr::Field { base, .. } => self.infer_expr(*base, sig),
            Expr::Bracket { callee, args } => {
                // A fifo op (`f.Deq[]` / `f.Enq[x]`) can fail and mutates
                // the fifo: reads + writes + decides, conservatively.
                if let Some(fifo) = self.fifo_op_target(*callee) {
                    sig.decides = true;
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
                    sig.decides |= callee_sig.decides;
                    sig.reads.extend(callee_sig.reads.iter().copied());
                    sig.writes.extend(callee_sig.writes.iter().copied());
                }
                self.infer_expr(*callee, sig);
                for arg in args {
                    self.infer_expr(*arg, sig);
                }
            }
            Expr::Spawn(inner) => self.infer_expr(*inner, sig),
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
        let elab = sig.allocates;
        for stmt in self.item_body(id).to_vec() {
            self.check_stmt(stmt, id, &sig, elab);
        }
        self.check_rows(id, &sig);
    }

    fn check_stmt(&mut self, id: StmtId, item: ItemId, sig: &EffectSig, elab: bool) {
        let span = self.ast.stmt_spans[id.0 as usize].clone();
        match self.ast.stmt(id).clone() {
            Stmt::Tick => {
                if !sig.suspends {
                    self.error(
                        span,
                        "`tick` requires `<suspends>` on the enclosing item".to_string(),
                    );
                }
            }
            Stmt::While { cond, body } => {
                if !sig.suspends && !sig.allocates {
                    self.error(
                        span,
                        "loop needs `<suspends>` (one iteration per cycle) or an \
                         elaboration-time bound under `<allocates>`"
                            .to_string(),
                    );
                }
                self.check_expr(cond, item, sig, elab);
                for s in body {
                    self.check_stmt(s, item, sig, elab);
                }
            }
            Stmt::Expr(e) => self.check_expr(e, item, sig, elab),
            Stmt::Assign { lhs, rhs } => {
                self.check_expr(lhs, item, sig, elab);
                self.check_expr(rhs, item, sig, elab);
            }
            Stmt::Let { init, .. } => self.check_expr(init, item, sig, elab),
            Stmt::Return(e) => {
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
                if !sig.suspends {
                    self.error(span, "`spawn` requires `<suspends>`".to_string());
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
                        span,
                        "fifo operations cannot fail at elaboration time".to_string(),
                    );
                }
                self.check_expr(callee, item, sig, elab);
                for arg in args {
                    self.check_expr(arg, item, sig, elab);
                }
            }
            Expr::Unary { operand, .. } => self.check_expr(operand, item, sig, elab),
            Expr::Binary { lhs, rhs, .. } => {
                self.check_expr(lhs, item, sig, elab);
                self.check_expr(rhs, item, sig, elab);
            }
            Expr::Field { base, .. } => self.check_expr(base, item, sig, elab),
            Expr::Ident(_) | Expr::Int(_) | Expr::Wildcard => {}
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
                    if !sig.choice {
                        self.error(
                            span,
                            "`any` is nondeterministic choice and requires `<choice>` \
                             (spec-only)"
                                .to_string(),
                        );
                    }
                }
                "sync" | "race" => {
                    if !sig.suspends {
                        self.error(span, format!("`{}` requires `<suspends>`", callee_def.name));
                    }
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
                if target == item && !sig.allocates {
                    self.error(
                        span,
                        format!(
                            "recursive call to `{}` requires `<allocates>` \
                             (elaboration-time recursion only)",
                            callee_def.name
                        ),
                    );
                    return;
                }
                let callee_sig = self.sigs[&target].clone();
                if callee_sig.suspends && !sig.suspends {
                    self.error(
                        span.clone(),
                        format!(
                            "calling `<suspends>` function `{}` requires `<suspends>`",
                            callee_def.name
                        ),
                    );
                }
                if callee_sig.allocates && !sig.allocates {
                    self.error(
                        span,
                        format!(
                            "calling `<allocates>` function `{}` requires `<allocates>`",
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

    /// State types and initializers are elaboration positions: no failure.
    fn check_elab_positions(&mut self) {
        let mut stack: Vec<ItemId> = self.ast.roots.clone();
        let empty = EffectSig {
            allocates: true,
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
                Item::Mem { ty, .. } | Item::Fifo { ty, .. } => {
                    self.check_expr(ty, id, &empty, true);
                }
                _ => {}
            }
        }
    }
}
