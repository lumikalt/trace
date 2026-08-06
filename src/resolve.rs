//! Name resolution: builds a definition arena and maps every identifier
//! expression to its definition in an `ExprId -> DefId` side table, per the
//! index-based-AST plan in DESIGN.md.
//!
//! Scoping rules:
//! - Items are mutually recursive within a scope (a rule may call a
//!   function declared after it), so each scope collects declarations
//!   before resolving bodies.
//! - `let` is the ONLY way to bind a fresh local, and may shadow. `x :=
//!   e` never declares — it's always either a write to an EXISTING
//!   definition (a reg/output/fifo/mem/inst port, or a `let`-bound
//!   local being reassigned) or, if `x` doesn't resolve at all, a clean
//!   "cannot find" error rather than silently declaring one. Splitting
//!   "define" (`let`) from "mutate" (`:=`) this way, rather than
//!   overloading `:=` for both like an earlier version of this language
//!   did, is a deliberate Verse-alignment choice (Verse itself keeps
//!   `x := e` a pure definition and requires `set x = e` for mutation) —
//!   it's what makes `pc := c` unambiguously a register write and
//!   `let a = m[pc]` unambiguously a local binding, instead of the two
//!   being distinguished only by whichever name happened to already be
//!   in scope.
//! - Names free in a function signature type (`bits[N]`) become implicit
//!   parameters of that function, per DESIGN.md's parameter inference.
//! - `reads`/`writes` arguments must name state (reg/mem/fifo); schedule
//!   directives must name rules.

use crate::ast::{Ast, Effect, Expr, ExprId, FnKind, Item, ItemId, Name, Stmt, StmtId};
use crate::lexer::Span;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DefId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefKind {
    Builtin,
    Module,
    /// `extmodule Name from "path.v" { ... }` — like `Module`, a legal
    /// `inst` target, but its own port list is plain data on the
    /// `Item::ExtModule` node (see `ast::ExtPort`), never separately
    /// `declare`d, matching `Struct`'s own fields.
    ExtModule,
    /// A `struct Name { field : ty, ... }` declaration — a type, not
    /// state; its own fields are never separately `declare`d (they're
    /// plain structural data on the `Item::Struct`, resolved contextually
    /// by types.rs, matching how a module's port names or a fifo's
    /// element type aren't standalone resolvable idents either).
    Struct,
    Reg,
    Mem,
    Fifo,
    Input,
    Output,
    /// An `io name : ty` structural port. Unlike every other `is_state()`
    /// kind, never legally read or written as a value anywhere — its only
    /// legal use is as an `Item::Attach` operand (see `resolve_attach_
    /// operand`, the one resolution path that doesn't reject it).
    Io,
    /// A `inst name : Module` child instance.
    Inst,
    /// One specific port of an `inst`, e.g. `c.a` — synthesized on first
    /// reference (see `Resolver::inst_port_def`) so that two rules
    /// touching different ports of the same instance don't conflict.
    InstPort,
    Rule,
    Fn,
    Spec,
    Impl,
    Param,
    /// Bound by appearing free in a function signature type.
    ImplicitParam,
    Local,
}

impl DefKind {
    pub fn is_state(self) -> bool {
        matches!(
            self,
            DefKind::Reg
                | DefKind::Mem
                | DefKind::Fifo
                | DefKind::Input
                | DefKind::Output
                | DefKind::Io
                | DefKind::Inst
                | DefKind::InstPort
        )
    }

    pub fn describe(self) -> &'static str {
        match self {
            DefKind::Builtin => "a builtin",
            DefKind::Module => "a module",
            DefKind::ExtModule => "an extmodule",
            DefKind::Struct => "a struct",
            DefKind::Reg => "a register",
            DefKind::Mem => "a memory",
            DefKind::Fifo => "a fifo",
            DefKind::Input => "an input port",
            DefKind::Output => "an output port",
            DefKind::Io => "an io port",
            DefKind::Inst => "a module instance",
            DefKind::InstPort => "a module instance port",
            DefKind::Rule => "a rule",
            DefKind::Fn => "a function",
            DefKind::Spec => "a spec",
            DefKind::Impl => "an impl",
            DefKind::Param => "a parameter",
            DefKind::ImplicitParam => "an implicit parameter",
            DefKind::Local => "a local",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Def {
    pub name: String,
    pub kind: DefKind,
    /// Definition site; empty for builtins.
    pub span: Span,
}

#[derive(Debug, Default)]
pub struct Resolution {
    pub defs: Vec<Def>,
    /// Every `Expr::Ident` maps to its definition.
    pub expr_defs: HashMap<ExprId, DefId>,
    /// `impl` item -> the spec it refines.
    pub refines: HashMap<ItemId, DefId>,
    /// Named item -> its definition (rules, fns, state, modules).
    pub item_defs: HashMap<ItemId, DefId>,
    /// Every def's immediately-enclosing module, `None` for one declared
    /// outside any module. `check_module_boundary` uses this to reject a
    /// state reference that crosses a module boundary at its own
    /// (lexical, one-time) resolution; firrtl.rs's `compile_call` reuses
    /// it to catch the same boundary crossing happening dynamically,
    /// through a call, to a state def the callee reads or writes.
    pub def_owner: HashMap<DefId, Option<ItemId>>,
    /// A `reads {a, b}`/`writes {a}` row argument's own name span -> the
    /// def it names, populated by `check_effect_args` at the same point it
    /// already looks the name up to validate it. A separate side table
    /// rather than folding into `expr_defs`: a row argument is a plain
    /// `Name`, never an `ExprId` (there's no expression there to attach
    /// one to) — keyed by `Span` rather than adding a dedicated id type
    /// for what is, so far, a single consumer (the language server's
    /// hover/go-to-definition, so `pc`/`mem` in `reads {pc, mem}` resolve
    /// like any other state reference instead of answering nothing).
    /// Populated for every name that resolves to SOME def, even one the
    /// surrounding match then rejects as the wrong kind (a rule named in
    /// `reads`, say) — the def is still real and still worth hovering,
    /// independent of whether using it there is legal.
    pub effect_arg_defs: HashMap<Span, DefId>,
}

impl Resolution {
    pub fn def(&self, id: DefId) -> &Def {
        &self.defs[id.0 as usize]
    }
}

/// A bare expression sitting alone at statement position implicitly
/// gates its enclosing rule/callee body — the same thing an explicit
/// `expr?` guard does — UNLESS it is already self-describing there: a
/// call (may or may not fail on its own terms; already has
/// independent, established bare-statement semantics that this must
/// not silently change), a fifo op (`Enq`/`Deq`, whose own fail
/// condition already folds into the guard on its own), a `spawn`
/// (fire-and-forget is the whole point of an unused handle, not a
/// condition to test), or a bracket-dispatched builtin (`sync[...]`/
/// `race[...]`, which use brackets for the same fallibility-marking
/// convention `Enq`/`Deq` do, and already have their own meaning as a
/// bare statement). Anything else sitting bare — `a = 1`, a
/// single bit-select `A[b]`, a bare boolean local — means the same
/// thing whether or not it's written with `?`; `types.rs` separately
/// enforces that it's actually `bits[1]` (the same check `if`/`while`
/// conditions already get), so this predicate itself needs no type
/// info and can run as early as effects.rs.
///
/// The single point every guard-placement/-folding question in the
/// compiler routes through — effects.rs (`fails` inference), types.rs
/// (the bits[1] check), and firrtl's guard-placement/-folding
/// (`checks.rs`, `calls.rs`, `writes.rs`) all call this rather than
/// each re-deriving "is this guard-like," the same rationale fifo.rs's
/// `rule_fifo_ops` documents for fifo-touch questions: independent
/// re-derivations drift out of agreement with each other.
pub fn is_guard_like(ast: &Ast, res: &Resolution, expr: ExprId) -> bool {
    match ast.expr(expr) {
        Expr::Guard(_) => true,
        Expr::Call { .. } | Expr::Spawn(_) => false,
        Expr::Bracket { callee, .. } => {
            // A builtin dispatched via brackets (`sync[h1, h2]`,
            // `race[h1, h2]`) already has its own established bare-
            // statement meaning — same exclusion rationale as `Call`,
            // just reached through the bracket-for-fallibility
            // convention instead of parens.
            if res
                .expr_defs
                .get(callee)
                .is_some_and(|d| res.def(*d).kind == DefKind::Builtin)
            {
                return false;
            }
            let Expr::Field { base, name } = ast.expr(*callee) else {
                return true;
            };
            let is_fifo_op = matches!(name.as_str(), "Enq" | "Deq")
                && res
                    .expr_defs
                    .get(base)
                    .is_some_and(|d| res.def(*d).kind == DefKind::Fifo);
            !is_fifo_op
        }
        _ => true,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveError {
    pub span: Span,
    pub message: String,
}

/// Callable names with no user definition. `sync`/`race` parse as idents;
/// `prio` is a priority encoder; `zext`/`sext` widen (zero/sign extend);
/// `popcount` counts set bits; `reverse` reverses bit order; `rotl`/
/// `rotr` rotate (constant or dynamic amount); `mux` is an explicit 2-way
/// combinational select; `max`/`min` are dual-purpose — compile-time-only
/// (usable in a type-position width expression, `bits[max(n, m)]`, same
/// as `clog2`) when every argument is itself compile-time-constant, but
/// a genuine synthesizable comparator+mux the moment at least one
/// argument is a real runtime `Bits` value; the rest are the primitive
/// vocabulary DESIGN.md examples assume.
/// `__race_value` is compiler-internal — lower.rs's own rendering of a
/// value-producing `race[...]`, never written by a user (see types.rs's
/// `type_builtin_call`). `logic` is NOT here — it's a real prefix
/// operator (`Expr::Logic`, ast.rs, a dedicated lexer keyword), not an
/// identifier resolved against this list the way the call-syntax
/// builtins below are.
const BUILTINS: &[&str] = &[
    "bits",
    "wire",
    "list",
    "any",
    "clog2",
    "pack",
    "trunc",
    "zext",
    "sext",
    "popcount",
    "reverse",
    "rotl",
    "rotr",
    "mux",
    "max",
    "min",
    "len",
    "sync",
    "race",
    "prio",
    "__race_value",
];

pub fn resolve(ast: &Ast) -> (Resolution, Vec<ResolveError>) {
    let mut resolver = Resolver {
        ast,
        res: Resolution::default(),
        errors: Vec::new(),
        scopes: vec![HashMap::new()],
        rule_scopes: vec![HashMap::new()],
        inst_ports: HashMap::new(),
        current_module: Vec::new(),
        declared_locals: Vec::new(),
        read_locals: HashSet::new(),
    };
    for name in BUILTINS {
        let id = resolver.new_def(name, DefKind::Builtin, 0..0);
        resolver.scopes[0].insert((*name).to_string(), id);
    }
    resolver.resolve_scope(&ast.roots.clone());
    (resolver.res, resolver.errors)
}

struct Resolver<'a> {
    ast: &'a Ast,
    res: Resolution,
    errors: Vec<ResolveError>,
    scopes: Vec<HashMap<String, DefId>>,
    /// A SEPARATE namespace for rule names, pushed/popped in lockstep with
    /// `scopes` at the ONE site rules can ever be declared (`Item::
    /// Module`'s own scope push/pop — a rule can't appear inside an `if`/
    /// `while`/`fn` body, so no other push/pop site needs a matching
    /// frame here). Rule names used to share `scopes` with everything
    /// else — purely incidental (every item kind funneled through one
    /// generic `declare`), not a deliberate choice; nothing anywhere
    /// (checked: no planned feature, no existing use) ever needs a rule
    /// name to resolve as an ordinary expression VALUE, only as a
    /// `schedule` directive name. Splitting them out is what unblocks
    /// `rule foo?` sugar's own `in foo` auto-declaration (see TODO.md) —
    /// before this, `foo` naming both the rule and the port had nowhere
    /// to put a second `DefId` under one shared key.
    rule_scopes: Vec<HashMap<String, DefId>>,
    /// (inst, port name) -> the synthesized `InstPort` resource for it,
    /// memoized so every `c.a` in the file shares one `DefId` (needed for
    /// effects.rs's conflict-set intersection to see them as the same
    /// resource).
    inst_ports: HashMap<(DefId, String), DefId>,
    /// Stack of enclosing `module` items, innermost last; empty at file
    /// root. Only pushed/popped around a module's own body, not its own
    /// declaration site (see `resolve_item`'s `Item::Module` arm).
    current_module: Vec<ItemId>,
    /// Every local declared via `let` or a fresh (unbound) `x := e`,
    /// anywhere in the file, in declaration order, with its own name+span
    /// for a good error message. Read unconditionally at every `declare`
    /// site rather than only inside a fn/impl body — a RULE-scoped slice
    /// of this is simply never checked (see `Item::Fn`'s own arm below),
    /// so recording rule-local declarations here too is harmless, and
    /// keeping ONE flat, unconditional list (no separate `current_fn`
    /// stack to push/pop/get wrong) is simpler than gating collection
    /// itself on where we are.
    declared_locals: Vec<(DefId, Name)>,
    /// Every local `DefId` ever READ (as an ordinary `Expr::Ident`, never
    /// an `Assign`'s own LHS — resolve_stmt's `Assign` arm never routes an
    /// `Expr::Ident` LHS through `resolve_expr`, so this can't double-count
    /// a write as a read). Checked, not just recorded, only for locals
    /// declared inside a fn/impl body (see `Item::Fn`'s own arm).
    read_locals: HashSet<DefId>,
}

impl<'a> Resolver<'a> {
    fn new_def(&mut self, name: &str, kind: DefKind, span: Span) -> DefId {
        self.res.defs.push(Def {
            name: name.to_string(),
            kind,
            span,
        });
        DefId(self.res.defs.len() as u32 - 1)
    }

    fn error(&mut self, span: Span, message: String) {
        self.errors.push(ResolveError { span, message });
    }

    /// True if `def` (assumed state, i.e. `is_state()`) belongs to the
    /// innermost enclosing module — false and errors otherwise. Modules
    /// share no state with each other, only ports, so a name declared in
    /// one module (or at file scope, outside any module) must not
    /// resolve inside a different one, however it's lexically nested.
    /// (A `Module` name itself is exempt from this — see
    /// `resolve_inst_target`, which deliberately bypasses this check.)
    fn check_module_boundary(&mut self, def: DefId, span: Span, text: &str) -> bool {
        if self.res.def_owner.get(&def).copied().flatten() == self.current_module.last().copied() {
            return true;
        }
        self.error(
            span,
            format!(
                "cannot use `{text}` here: it belongs to a different module (modules \
                 share no state with each other, only ports declared on themselves)"
            ),
        );
        false
    }

    /// The `InstPort` resource for `inst`'s `port`, allocating one on
    /// first reference and reusing it for every later `inst.port` in the
    /// file (see `inst_ports`'s doc comment for why that sharing matters).
    fn inst_port_def(&mut self, inst: DefId, port: &str) -> DefId {
        if let Some(&def) = self.inst_ports.get(&(inst, port.to_string())) {
            return def;
        }
        let base = self.res.def(inst);
        let full_name = format!("{}.{port}", base.name);
        let span = base.span.clone();
        let def = self.new_def(&full_name, DefKind::InstPort, span);
        self.inst_ports.insert((inst, port.to_string()), def);
        def
    }

    fn lookup(&self, text: &str) -> Option<DefId> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(text).copied())
    }

    /// Declare in the innermost scope. Locals and implicit parameters may
    /// shadow silently; everything else duplicated in the same scope is an
    /// error (the previous definition wins for later references).
    fn declare(&mut self, name: &Name, kind: DefKind) -> DefId {
        let id = self.new_def(&name.text, kind, name.span.clone());
        let scope = self.scopes.last_mut().unwrap();
        if let Some(prev) = scope.get(&name.text).copied()
            && !matches!(kind, DefKind::Local | DefKind::ImplicitParam)
        {
            let prev_kind = self.res.def(prev).kind;
            self.error(
                name.span.clone(),
                format!(
                    "`{}` is already defined in this scope as {}",
                    name.text,
                    prev_kind.describe()
                ),
            );
            return id;
        }
        self.scopes
            .last_mut()
            .unwrap()
            .insert(name.text.clone(), id);
        id
    }

    fn lookup_rule(&self, text: &str) -> Option<DefId> {
        self.rule_scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(text).copied())
    }

    /// `declare`'s own rule-namespace twin — see `rule_scopes`'s doc
    /// comment. Every def this ever creates is `DefKind::Rule`, so
    /// (unlike `declare`) the duplicate-name error can just say "already
    /// a rule" without needing to describe the previous def's kind.
    fn declare_rule(&mut self, name: &Name) -> DefId {
        let id = self.new_def(&name.text, DefKind::Rule, name.span.clone());
        let scope = self.rule_scopes.last_mut().unwrap();
        if scope.contains_key(&name.text) {
            self.error(
                name.span.clone(),
                format!("`{}` is already defined in this scope as a rule", name.text),
            );
            return id;
        }
        self.rule_scopes
            .last_mut()
            .unwrap()
            .insert(name.text.clone(), id);
        id
    }

    /// Two-phase scope resolution: declare all items first, then resolve
    /// bodies, so items are mutually recursive.
    fn resolve_scope(&mut self, items: &[ItemId]) {
        for item in items {
            self.collect_decl(*item);
        }
        for item in items {
            self.resolve_item(*item);
        }
    }

    fn collect_decl(&mut self, id: ItemId) {
        // A rule's own name lives in `rule_scopes`, not `scopes` — see
        // that field's doc comment — so it needs `declare_rule`, not the
        // generic path every other item kind below shares.
        if let Item::Rule { name, .. } = self.ast.item(id) {
            let name = name.clone();
            let def = self.declare_rule(&name);
            self.res
                .def_owner
                .insert(def, self.current_module.last().copied());
            self.res.item_defs.insert(id, def);
            return;
        }
        let (name, kind) = match self.ast.item(id) {
            Item::Module { name, .. } => (name.clone(), DefKind::Module),
            Item::ExtModule { name, .. } => (name.clone(), DefKind::ExtModule),
            Item::Reg { name, .. } => (name.clone(), DefKind::Reg),
            Item::Mem { name, .. } => (name.clone(), DefKind::Mem),
            Item::Fifo { name, .. } => (name.clone(), DefKind::Fifo),
            Item::Input { name, .. } => (name.clone(), DefKind::Input),
            Item::Output { name, .. } => (name.clone(), DefKind::Output),
            Item::Io { name, .. } => (name.clone(), DefKind::Io),
            Item::Attach { .. } => return,
            Item::Inst { name, .. } => (name.clone(), DefKind::Inst),
            Item::Rule { .. } => unreachable!("handled above"),
            Item::Fn { name, kind, .. } => {
                let def_kind = match kind {
                    FnKind::Fn => DefKind::Fn,
                    FnKind::Spec => DefKind::Spec,
                    FnKind::Impl { .. } => DefKind::Impl,
                };
                (name.clone(), def_kind)
            }
            Item::Struct { name, .. } => (name.clone(), DefKind::Struct),
            Item::Schedule { .. } => return,
            Item::Invariant { .. } => return,
        };
        let def = self.declare(&name, kind);
        self.res
            .def_owner
            .insert(def, self.current_module.last().copied());
        self.res.item_defs.insert(id, def);
    }

    fn resolve_item(&mut self, id: ItemId) {
        match self.ast.item(id) {
            Item::Module { items, .. } => {
                self.current_module.push(id);
                self.scopes.push(HashMap::new());
                // Pushed/popped in lockstep with `scopes` above — the
                // only site a rule can ever be declared under (see
                // `rule_scopes`'s doc comment).
                self.rule_scopes.push(HashMap::new());
                self.resolve_scope(&items.clone());
                self.rule_scopes.pop();
                self.scopes.pop();
                self.current_module.pop();
            }
            Item::Reg {
                ty,
                init,
                bound,
                lower,
                ..
            } => {
                self.resolve_expr(*ty, false);
                if let Some(init) = init {
                    self.resolve_expr(*init, false);
                }
                if let Some(bound) = bound {
                    self.resolve_expr(*bound, false);
                    if let Some(&def) = self.res.item_defs.get(&id) {
                        self.check_bound_self_reference(def, *bound);
                    }
                }
                if let Some(lower) = lower {
                    self.resolve_expr(*lower, false);
                }
            }
            Item::Mem {
                ty, bound, lower, ..
            } => {
                self.resolve_expr(*ty, false);
                // v17: mirrors `Item::Fn`'s own `ret_bound` handling
                // below exactly -- the self-reference placeholder
                // (`elem`) is never a real scoped binding (a mem
                // element has no `DefId` of its own), so only the
                // bound's own constant RHS is resolved normally; the
                // placeholder itself is checked by TEXT in `check_mem_
                // bound_shape`, not passed through `resolve_expr`
                // (which would otherwise error "cannot find `elem`").
                if let Some(bound) = bound {
                    self.check_mem_bound_shape(*bound);
                    if let Expr::Binary { rhs, .. } = self.ast.expr(*bound).clone() {
                        self.resolve_expr(rhs, false);
                    }
                }
                if let Some(lower) = lower {
                    self.resolve_expr(*lower, false);
                }
            }
            Item::Fifo { ty, .. } | Item::Input { ty, .. } | Item::Io { ty, .. } => {
                self.resolve_expr(*ty, false);
            }
            // The one legal place to reference an `io` port: `resolve_
            // attach_operand`, unlike `resolve_expr`, doesn't reject
            // `DefKind::Io`.
            Item::Attach { a, b } => {
                self.resolve_attach_operand(*a);
                self.resolve_attach_operand(*b);
            }
            // Field names are structural, not scoped idents (matching a
            // module's port names or a fifo's element type) — only each
            // field's own TYPE expression resolves against scope. No
            // implicit-param binding (`in_type: false`): a struct
            // declaration has no call site to infer a generic width
            // from, unlike a fn's params.
            //
            // v18: a field's own `where` bound mirrors `Item::Mem`'s own
            // handling exactly -- the placeholder (`_`) is never a real
            // scoped binding, so only the bound's constant RHS resolves
            // normally; the placeholder itself is checked by SHAPE in
            // `check_struct_field_bound_shape`, not passed through
            // `resolve_expr`.
            Item::Struct { fields, .. } => {
                for field in fields.clone() {
                    self.resolve_expr(field.ty, false);
                    if let Some(bound) = field.bound {
                        self.check_struct_field_bound_shape(bound);
                        if let Expr::Binary { rhs, .. } = self.ast.expr(bound).clone() {
                            self.resolve_expr(rhs, false);
                        }
                    }
                    if let Some(lower) = field.lower {
                        self.resolve_expr(lower, false);
                    }
                }
            }
            // Same shape as `Struct`'s fields: an extmodule's ports are
            // structural data, not separately declared idents — only
            // each port's own TYPE expression resolves against scope.
            Item::ExtModule { ports, .. } => {
                for port in ports.clone() {
                    self.resolve_expr(port.ty, false);
                }
            }
            Item::Output {
                ty,
                init,
                bound,
                lower,
                ..
            } => {
                self.resolve_expr(*ty, false);
                if let Some(init) = init {
                    self.resolve_expr(*init, false);
                }
                if let Some(bound) = bound {
                    self.resolve_expr(*bound, false);
                    if let Some(&def) = self.res.item_defs.get(&id) {
                        self.check_bound_self_reference(def, *bound);
                    }
                }
                if let Some(lower) = lower {
                    self.resolve_expr(*lower, false);
                }
            }
            Item::Inst { module, .. } => {
                self.resolve_inst_target(*module);
                if let Some(def) = self.res.expr_defs.get(module).copied()
                    && !matches!(self.res.def(def).kind, DefKind::Module | DefKind::ExtModule)
                {
                    self.error(
                        self.ast.expr_spans[module.0 as usize].clone(),
                        format!(
                            "`{}` is {}, not a module or extmodule",
                            self.res.def(def).name,
                            self.res.def(def).kind.describe()
                        ),
                    );
                }
            }
            Item::Rule { effects, body, .. } => {
                self.check_effect_args(&effects.clone());
                self.scopes.push(HashMap::new());
                self.resolve_stmts(&body.clone());
                self.scopes.pop();
            }
            Item::Fn {
                kind,
                params,
                ret,
                ret_bound,
                ret_lower,
                effects,
                body,
                ..
            } => {
                if let FnKind::Impl { refines } = kind {
                    match self.lookup(&refines.text) {
                        None => self.error(
                            refines.span.clone(),
                            format!("cannot find spec `{}`", refines.text),
                        ),
                        Some(def) if self.res.def(def).kind != DefKind::Spec => self.error(
                            refines.span.clone(),
                            format!(
                                "`{}` is {}, not a spec",
                                refines.text,
                                self.res.def(def).kind.describe()
                            ),
                        ),
                        Some(def) => {
                            self.res.refines.insert(id, def);
                        }
                    }
                }
                let params = params.clone();
                let ret = *ret;
                let ret_bound = *ret_bound;
                let ret_lower = *ret_lower;
                let effects = effects.clone();
                let body = body.clone();
                self.scopes.push(HashMap::new());
                // Signature types first: free names bind as implicit
                // params, so `reqs : bits[N]` introduces `N` before the
                // body resolves.
                for param in &params {
                    self.resolve_expr(param.ty, true);
                    let param_def = self.declare(&param.name, DefKind::Param);
                    // v12: a param's own `where` bound, checked as a
                    // call-site obligation by `bounds.rs` -- resolved
                    // AFTER `declare` so the self-reference check below
                    // has the param's own `DefId` in hand already (no
                    // `item_defs` lookup needed, unlike reg/out, since
                    // a param has no `ItemId` of its own).
                    if let Some(bound) = param.bound {
                        self.resolve_expr(bound, false);
                        self.check_bound_self_reference(param_def, bound);
                    }
                    if let Some(lower) = param.lower {
                        self.resolve_expr(lower, false);
                    }
                }
                if let Some(ret) = ret {
                    self.resolve_expr(ret, true);
                }
                // v13: a return bound's self-reference position is
                // never a real scoped binding (there's no `DefId` for
                // "the return value" the way a param/reg/out already
                // has one) -- only its constant side(s) are resolved
                // normally; the placeholder `_` is checked by SHAPE
                // in `check_ret_bound_shape`, not passed through
                // `resolve_expr` (which no-ops on `Expr::Wildcard`
                // anyway, but the shape check runs regardless). A bound
                // with no declared `ret` type has nothing to check its
                // own width against (bounds.rs needs `ret`'s own type
                // expression) -- caught here so it's a real diagnostic,
                // not an internal panic later.
                if let Some(ret_bound) = ret_bound {
                    if ret.is_none() {
                        self.error(
                            self.ast.expr_spans[ret_bound.0 as usize].clone(),
                            "a return bound needs a declared return type to check against (e.g. \
                             `: [8] where _ < 20`)"
                                .to_string(),
                        );
                    }
                    self.check_ret_bound_shape(ret_bound);
                    if let Expr::Binary { rhs, .. } = self.ast.expr(ret_bound).clone() {
                        self.resolve_expr(rhs, false);
                    }
                }
                if let Some(ret_lower) = ret_lower {
                    self.resolve_expr(ret_lower, false);
                }
                self.check_effect_args(&effects);
                let locals_before = self.declared_locals.len();
                self.resolve_stmts(&body);
                // A `let`-declared local ANYWHERE in this fn/impl body, at
                // any nesting depth, and never read back is dead by
                // construction: a fn's only observable outputs are its
                // return value and its state writes, and a local (unlike
                // state) is invisible outside this body, so nothing later
                // can read it either. (This check no longer needs to worry
                // about `log := d` on some unbound name meaning to reach a
                // module's `log` reg — that's caught earlier, and more
                // precisely, by `resolve_stmt`'s `Stmt::Assign` arm's own
                // "cannot find" error, since `x := e` never declares a
                // fresh local anymore; see this file's own module doc
                // comment.) Rule bodies are deliberately NOT checked here
                // (only inside this `Item::Fn` arm) — a rule-level scratch
                // local via bare `let x = e` is a legitimate, load-bearing
                // pattern (reassignment across statements via `x := e`),
                // not a fn's-only-two-outputs invariant.
                let newly_declared = self.declared_locals[locals_before..].to_vec();
                for (def, name) in &newly_declared {
                    if !self.read_locals.contains(def) {
                        self.error(
                            name.span.clone(),
                            format!(
                                "`{}` is assigned but never read in this fn/impl body \
                                 (v0 restriction: a fn/impl's only outputs are its return \
                                 value and its state writes, and a local is invisible \
                                 outside its own body); use a fn/impl declared inside that \
                                 module if this was meant to write its state",
                                name.text
                            ),
                        );
                    }
                }
                self.scopes.pop();
            }
            Item::Schedule { directives } => {
                let names: Vec<Name> = directives
                    .iter()
                    .flat_map(|d| match d {
                        crate::ast::ScheduleDirective::Urgency(ns) => ns.clone(),
                        crate::ast::ScheduleDirective::MutuallyExclusive(ns) => ns.clone(),
                        crate::ast::ScheduleDirective::ConflictFree(ns) => ns.clone(),
                    })
                    .collect();
                // `rule_scopes` can only ever contain `DefKind::Rule`
                // defs (only `declare_rule` ever inserts into it), so
                // there's no "found something, but it's the wrong kind"
                // case to check anymore — `lookup_rule` finding anything
                // at all already proves it's a rule.
                for name in names {
                    if self.lookup_rule(&name.text).is_none() {
                        self.error(
                            name.span.clone(),
                            format!("cannot find rule `{}`", name.text),
                        );
                    }
                }
            }
            // Every ident inside an `invariant` expression already names
            // a real, in-scope `reg`/`out` (there's no self-reference
            // placeholder to special-case, unlike a `Reg`/`Mem`/`Fn`
            // bound) -- an ordinary expression resolve. `bounds.rs`'s own
            // `collect_relational_bounds` does the shape recognition and
            // reports anything it doesn't recognize.
            Item::Invariant { expr } => {
                self.resolve_expr(*expr, false);
            }
        }
    }

    /// `reads {a, b}` / `writes {a}` arguments must name state.
    fn check_effect_args(&mut self, effects: &[Effect]) {
        for effect in effects {
            for arg in &effect.args {
                let looked_up = self.lookup(&arg.text);
                if let Some(def) = looked_up {
                    self.res.effect_arg_defs.insert(arg.span.clone(), def);
                }
                match looked_up {
                    // A rule name is never in `scopes` at all now (see
                    // `rule_scopes`), so it falls through to `None` here
                    // like any other unknown name -- checked separately
                    // for a clearer message than a bare "cannot find".
                    None if self.lookup_rule(&arg.text).is_some() => self.error(
                        arg.span.clone(),
                        format!(
                            "`{}` is a rule; rules aren't state and can't be named in \
                             `reads`/`writes`",
                            arg.text
                        ),
                    ),
                    None => self.error(
                        arg.span.clone(),
                        format!("cannot find state `{}`", arg.text),
                    ),
                    Some(def) if self.res.def(def).kind == DefKind::Io => self.error(
                        arg.span.clone(),
                        format!(
                            "`{}` is an io port; io ports carry no value and can't be named \
                             in `reads`/`writes` (only `attach` touches one, and it's never \
                             part of a rule)",
                            arg.text
                        ),
                    ),
                    Some(def) if !self.res.def(def).kind.is_state() => self.error(
                        arg.span.clone(),
                        format!(
                            "`{}` is {}, not state (reg, mem, fifo, in, out, or inst)",
                            arg.text,
                            self.res.def(def).kind.describe()
                        ),
                    ),
                    Some(def) => {
                        self.check_module_boundary(def, arg.span.clone(), &arg.text);
                    }
                }
            }
        }
    }

    fn resolve_stmts(&mut self, stmts: &[StmtId]) {
        for stmt in stmts {
            self.resolve_stmt(*stmt);
        }
    }

    fn resolve_stmt(&mut self, id: StmtId) {
        match self.ast.stmt(id).clone() {
            Stmt::Expr(expr) => self.resolve_expr(expr, false),
            Stmt::Assign { lhs, rhs } => {
                // Right side first: `x := x + 1` with a fresh `x` is an
                // error on the right, not a self-reference.
                self.resolve_expr(rhs, false);
                match self.ast.expr(lhs) {
                    Expr::Ident(text) => {
                        if let Some(def) = self.lookup(text) {
                            let in_scope = !self.res.def(def).kind.is_state()
                                || self.check_module_boundary(
                                    def,
                                    self.ast.expr_spans[lhs.0 as usize].clone(),
                                    text,
                                );
                            if in_scope {
                                if self.res.def(def).kind == DefKind::Input {
                                    self.error(
                                        self.ast.expr_spans[lhs.0 as usize].clone(),
                                        format!(
                                            "cannot assign to `{text}`: it is an input port \
                                             (inputs are read-only, driven from outside the \
                                             module)"
                                        ),
                                    );
                                } else if self.res.def(def).kind == DefKind::Io {
                                    self.error(
                                        self.ast.expr_spans[lhs.0 as usize].clone(),
                                        format!(
                                            "cannot assign to `{text}`: it is an io port (io \
                                             ports carry no value — the only legal use is \
                                             `attach`ing it to another io port)"
                                        ),
                                    );
                                } else if self.res.def(def).kind == DefKind::Inst {
                                    self.error(
                                        self.ast.expr_spans[lhs.0 as usize].clone(),
                                        format!(
                                            "cannot assign to `{text}` directly: it is a \
                                             module instance; write a specific port instead \
                                             (`{text}.port := ...`)"
                                        ),
                                    );
                                } else if self.res.def(def).kind == DefKind::Builtin {
                                    self.error(
                                        self.ast.expr_spans[lhs.0 as usize].clone(),
                                        format!(
                                            "cannot assign to `{text}`: it is a builtin, not a \
                                             declared local; pick a different name (or use \
                                             `let {text} = ...` inside this rule to shadow it)"
                                        ),
                                    );
                                }
                                self.res.expr_defs.insert(lhs, def);
                            }
                        } else {
                            self.error(
                                self.ast.expr_spans[lhs.0 as usize].clone(),
                                format!(
                                    "cannot find `{text}`; use `let {text} = ...` to declare \
                                     a new local"
                                ),
                            );
                        }
                    }
                    _ => self.resolve_expr(lhs, false),
                }
            }
            Stmt::Let { name, init } => {
                self.resolve_expr(init, false);
                let def = self.declare(&name, DefKind::Local);
                self.declared_locals.push((def, name));
            }
            Stmt::Tick => {}
            Stmt::Break => {}
            Stmt::Return(expr) => {
                if let Some(expr) = expr {
                    self.resolve_expr(expr, false);
                }
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                self.resolve_expr(cond, false);
                self.scopes.push(HashMap::new());
                self.resolve_stmts(&then_body);
                self.scopes.pop();
                if let Some(else_body) = else_body {
                    self.scopes.push(HashMap::new());
                    self.resolve_stmts(&else_body);
                    self.scopes.pop();
                }
            }
            Stmt::IfLet {
                name,
                init,
                then_body,
                else_body,
            } => {
                // `init` resolves in the OUTER scope (same as `If`'s
                // `cond`) -- `name` isn't in scope for its own
                // initializer. `name` is then declared INSIDE the
                // then_body's own pushed scope, so it's invisible to
                // `else_body` and to anything after the whole statement
                // for free, the same way `If`'s existing then/else scope
                // push/pop already isolates a plain `let` declared inside
                // one branch from the other -- no new scoping mechanism
                // needed, just declaring `name` one level deeper than
                // `If`'s `cond` does.
                self.resolve_expr(init, false);
                self.scopes.push(HashMap::new());
                let def = self.declare(&name, DefKind::Local);
                self.declared_locals.push((def, name));
                self.resolve_stmts(&then_body);
                self.scopes.pop();
                if let Some(else_body) = else_body {
                    self.scopes.push(HashMap::new());
                    self.resolve_stmts(&else_body);
                    self.scopes.pop();
                }
            }
            Stmt::While { cond, body } => {
                self.resolve_expr(cond, false);
                self.scopes.push(HashMap::new());
                self.resolve_stmts(&body);
                self.scopes.pop();
            }
            Stmt::WhileLet { name, init, body } => {
                // Same shape as `IfLet`'s own arm above, one scope-push
                // deeper than `While`'s `cond` — `name` is declared
                // INSIDE `body`'s own pushed scope, invisible after the
                // loop, for the same reason `IfLet`'s `name` is
                // invisible outside `then_body`.
                self.resolve_expr(init, false);
                self.scopes.push(HashMap::new());
                let def = self.declare(&name, DefKind::Local);
                self.declared_locals.push((def, name));
                self.resolve_stmts(&body);
                self.scopes.pop();
            }
        }
    }

    /// The `Module` name in `inst name : Module`: unlike every other
    /// identifier use, this one deliberately walks past a module
    /// boundary — a nested module's name is only visible to its
    /// immediate lexical parent's `inst`, which is exactly what a plain
    /// scope lookup already gives (see `resolve_item`'s `Item::Module`
    /// arm), with no `check_module_boundary` call to undo it.
    fn resolve_inst_target(&mut self, id: ExprId) {
        let Expr::Ident(text) = self.ast.expr(id).clone() else {
            self.resolve_expr(id, false);
            return;
        };
        if let Some(def) = self.lookup(&text) {
            self.res.expr_defs.insert(id, def);
        } else {
            let span = self.ast.expr_spans[id.0 as usize].clone();
            self.error(span, format!("cannot find `{text}`"));
        }
    }

    /// An `attach` operand: a bare `io` port name, or `inst.port` (an
    /// instance's `io` port, checked further in types.rs via `find_port`
    /// the same way an instance's `in`/`out` ports already are). Bare
    /// names resolve here directly rather than through `resolve_expr`,
    /// since that generic path rejects `DefKind::Io` outright — this is
    /// the one place that must NOT.
    fn resolve_attach_operand(&mut self, id: ExprId) {
        let Expr::Ident(text) = self.ast.expr(id).clone() else {
            // `inst.port` and anything else: the generic Field arm
            // already resolves `inst.port` without an Io-specific ban
            // (deferred to types.rs, same as Input/Output); any other
            // shape resolves fine too and is rejected as the wrong shape
            // in types.rs's own attach check.
            self.resolve_expr(id, false);
            return;
        };
        let span = self.ast.expr_spans[id.0 as usize].clone();
        let Some(def) = self.lookup(&text) else {
            self.error(span, format!("cannot find `{text}`"));
            return;
        };
        // `is_state()` is true for every def this fn accepts (Io) and
        // every wrong-kind def worth a boundary check too; only a
        // genuinely non-state name (a rule, a fn, ...) skips it, matching
        // `resolve_expr`'s own `in_scope` guard above.
        if self.res.def(def).kind.is_state()
            && !self.check_module_boundary(def, span.clone(), &text)
        {
            return;
        }
        if self.res.def(def).kind != DefKind::Io {
            self.error(
                span,
                format!(
                    "`{text}` cannot be attached: it is {}, not an io port",
                    self.res.def(def).kind.describe()
                ),
            );
            return;
        }
        self.res.expr_defs.insert(id, def);
    }

    /// `in_type`: name misses bind implicit parameters instead of erroring
    /// (only signature types pass true).
    fn resolve_expr(&mut self, id: ExprId, in_type: bool) {
        match self.ast.expr(id).clone() {
            Expr::Ident(text) => {
                if let Some(def) = self.lookup(&text) {
                    let ok = !self.res.def(def).kind.is_state()
                        || self.check_module_boundary(
                            def,
                            self.ast.expr_spans[id.0 as usize].clone(),
                            &text,
                        );
                    if ok && self.res.def(def).kind == DefKind::Io {
                        self.error(
                            self.ast.expr_spans[id.0 as usize].clone(),
                            format!(
                                "cannot use `{text}` as a value: it is an io port (io ports \
                                 carry no value — the only legal use is `attach`ing it to \
                                 another io port)"
                            ),
                        );
                    } else if ok {
                        self.res.expr_defs.insert(id, def);
                        self.read_locals.insert(def);
                    }
                } else if in_type {
                    let name = Name {
                        text: text.clone(),
                        span: self.ast.expr_spans[id.0 as usize].clone(),
                    };
                    let def = self.declare(&name, DefKind::ImplicitParam);
                    self.res.expr_defs.insert(id, def);
                } else {
                    let span = self.ast.expr_spans[id.0 as usize].clone();
                    self.error(span, format!("cannot find `{text}`"));
                }
            }
            Expr::Int(_) | Expr::SizedInt { .. } | Expr::Wildcard => {}
            Expr::Unary { operand, .. } => self.resolve_expr(operand, in_type),
            Expr::Binary { lhs, rhs, .. } => {
                self.resolve_expr(lhs, in_type);
                self.resolve_expr(rhs, in_type);
            }
            Expr::Guard(inner) => self.resolve_expr(inner, in_type),
            // Field names are structural; only the base resolves against
            // scope. When the base is an `inst`, the field access itself
            // also gets a resource: `c.a` and `c.b` must stay distinct
            // conflict-wise, so a rule writing one doesn't stall a rule
            // writing the other (see `inst_port_def`).
            Expr::Field { base, name } => {
                self.resolve_expr(base, in_type);
                if let Some(&base_def) = self.res.expr_defs.get(&base)
                    && self.res.def(base_def).kind == DefKind::Inst
                {
                    let port_def = self.inst_port_def(base_def, &name);
                    self.res.expr_defs.insert(id, port_def);
                }
            }
            Expr::Call { callee, args } | Expr::Bracket { callee, args } => {
                self.resolve_expr(callee, in_type);
                for arg in args {
                    self.resolve_expr(arg, in_type);
                }
            }
            Expr::Spawn(inner) => self.resolve_expr(inner, in_type),
            Expr::ListLit(items) => {
                for item in items {
                    self.resolve_expr(item, in_type);
                }
            }
            Expr::Range { lo, hi } => {
                if let Some(lo) = lo {
                    self.resolve_expr(lo, in_type);
                }
                if let Some(hi) = hi {
                    self.resolve_expr(hi, in_type);
                }
            }
            Expr::Or(alts) => {
                for alt in alts {
                    self.resolve_expr(alt, in_type);
                }
            }
            Expr::StructLit { name, fields, base } => {
                self.resolve_expr(name, in_type);
                if let Some(def) = self.res.expr_defs.get(&name).copied()
                    && self.res.def(def).kind != DefKind::Struct
                {
                    self.error(
                        self.ast.expr_spans[name.0 as usize].clone(),
                        format!(
                            "`{}` is {}, not a struct",
                            self.res.def(def).name,
                            self.res.def(def).kind.describe()
                        ),
                    );
                }
                for (_, value) in fields {
                    self.resolve_expr(value, in_type);
                }
                if let Some(base) = base {
                    self.resolve_expr(base, in_type);
                }
            }
            // `?T`'s `T` is unconditionally a type position, regardless
            // of the context `OptionTy` itself was reached from.
            Expr::OptionTy(inner) => self.resolve_expr(inner, true),
            Expr::Absent => {}
            // `optional <expr>`'s `<expr>` is a value, not a type —
            // resolves in whatever context `Optional` itself was reached
            // from, like `Unary`/`Guard`/`Spawn` above, unlike `OptionTy`.
            Expr::Optional(inner) => self.resolve_expr(inner, in_type),
            // `logic <expr>`'s `<expr>` is a value, not a type — same
            // pass-through as `Optional` above.
            Expr::Logic(inner) => self.resolve_expr(inner, in_type),
        }
    }

    /// A `reg`/`out`/param's `where` bound must constrain the SAME def
    /// it's declared on — `where other < 10` says nothing about THIS
    /// def's own value, so `bounds.rs`'s induction argument (which only
    /// ever checks writes/call arguments against the declared def)
    /// would have nothing to reason about. The full shape (`Binary {
    /// Lt, .. }`, a compile-time-constant RHS) is validated later, in
    /// `bounds.rs` — this only checks self-reference, since that's a
    /// resolve-time (DefId-level) question. A malformed shape (LHS not
    /// even an `Ident`, or one that fails to resolve at all) falls
    /// through to the same "doesn't match" error, which is fine:
    /// `bounds.rs` reports the precise shape complaint separately.
    /// Takes the `DefId` directly (not an `ItemId`) so a param's own
    /// def (returned straight from `declare`, v12 — no `item_defs`
    /// entry exists for a param) can call this identically to a
    /// reg/out's own `item_defs`-derived def.
    ///
    /// Stage 3's own `_`-placeholder unification: the wildcard `_` is
    /// now accepted here too, alongside the literal name — mem-element/
    /// return/struct-field bounds (`check_mem_bound_shape`/`check_ret_
    /// bound_shape`/`check_struct_field_bound_shape` below) have
    /// required `_` since their own v18 retrofit, since none of them has
    /// a real `DefId` to compare against; reg/out/param DO have one, so
    /// this was never retrofitted to match, leaving `_` rejected here
    /// specifically until now. The literal-name form stays accepted
    /// too, deliberately — every existing test/example uses it, and
    /// `bounds.rs`'s own collector (`collect_one_bounded_def` etc.)
    /// never reads `lhs` at all once this check passes, so there is no
    /// downstream reason to prefer one form over the other.
    ///
    /// Stage 3's own operator generalization (`parser.rs`'s `parse_
    /// where_bound`, v21): the self-reference may now be on EITHER side
    /// of the comparison, not just `lhs` — `parse_where_bound` doesn't
    /// distinguish which side the user wrote self on, it just parses
    /// positionally, so `where 5 > cnt` produces `Binary { Gt, lhs: 5,
    /// rhs: cnt }` exactly as validly as `where cnt < 5` does, and both
    /// must be accepted (`bounds.rs`'s own collector normalizes either
    /// shape into the same `(lower, upper)` — see its own doc comment).
    /// This check doesn't distinguish the two-sided `L <= i < K` form
    /// (where self is positionally fixed at the MIDDLE operand, i.e.
    /// `lhs`, not commutable) from the one-sided form here — checking
    /// either side is very slightly too permissive for a malformed
    /// two-sided bound with self on the wrong side (`where 0 <= 5 <
    /// cnt`), but that shape still gets a real, correct rejection from
    /// `types.rs`'s own `check_where_bound_init` (its `const_eval` on
    /// `rhs` fails when `rhs` is a bare `Ident` under an empty env), so
    /// this is a slightly less specific error message on an already-
    /// wrong program, never a silently-accepted one.
    fn check_bound_self_reference(&mut self, self_def: DefId, bound: ExprId) {
        let Expr::Binary { lhs, rhs, .. } = self.ast.expr(bound).clone() else {
            return;
        };
        let is_self = |e: ExprId, this: &Self| {
            matches!(this.ast.expr(e), Expr::Wildcard)
                || this.res.expr_defs.get(&e).copied() == Some(self_def)
        };
        if !is_self(lhs, self) && !is_self(rhs, self) {
            self.error(
                self.ast.expr_spans[lhs.0 as usize].clone(),
                "a `where` bound must reference the same reg/out/param it's declared on \
                 (either by name, or via `_`)"
                    .to_string(),
            );
        }
    }

    /// v13's own self-reference check, but for a fn's return bound --
    /// unlike `check_bound_self_reference`, there's no existing `DefId`
    /// to compare against (a return value is never a named binding
    /// anywhere in scope), so this checks the bound's self-reference
    /// position by SHAPE instead: it must be the wildcard `_`
    /// (`Expr::Wildcard` -- a reserved AST node, not a generic
    /// identifier, see `ast.rs`/`lexer.rs`; v18 retrofit, was the
    /// literal identifier `result` through v17). Same fallthrough
    /// reasoning as `check_bound_self_reference`: a malformed shape
    /// (LHS not even present) falls through to the same "doesn't match"
    /// error; `bounds.rs` reports the precise shape complaint
    /// separately.
    fn check_ret_bound_shape(&mut self, bound: ExprId) {
        let Expr::Binary { lhs, rhs, .. } = self.ast.expr(bound).clone() else {
            return;
        };
        // Stage 3's own operator generalization (see `check_bound_self_
        // reference`'s own doc comment for the full reasoning): `_` may
        // now be on either side, since the parser parses positionally
        // and doesn't know which operand is meant as self.
        let is_placeholder = matches!(self.ast.expr(lhs), Expr::Wildcard)
            || matches!(self.ast.expr(rhs), Expr::Wildcard);
        if !is_placeholder {
            self.error(
                self.ast.expr_spans[lhs.0 as usize].clone(),
                "a return bound must reference the return value via `_` (e.g. `where _ < 20`)"
                    .to_string(),
            );
        }
    }

    /// v17's own sibling of `check_ret_bound_shape` immediately above --
    /// same reasoning, same shape: a mem's element has no scoped `DefId`
    /// either (unlike a reg/out/param's own bound, checked by `check_
    /// bound_self_reference` against a real binding), so the self-
    /// reference position is checked by SHAPE against `_`
    /// (`Expr::Wildcard`; v18 retrofit, was the literal identifier
    /// `elem` through v17), kept as its own small function rather than
    /// generalizing `check_ret_bound_shape` -- this file's own
    /// convention (see `bounds.rs`'s `collect_one_ret_bound` doc
    /// comment) is to only share a helper across near-identical
    /// branches within one function, not force a shared abstraction
    /// across two conceptually separate checks like these.
    fn check_mem_bound_shape(&mut self, bound: ExprId) {
        let Expr::Binary { lhs, rhs, .. } = self.ast.expr(bound).clone() else {
            return;
        };
        // Stage 3's own operator generalization -- see `check_bound_
        // self_reference`'s own doc comment.
        let is_placeholder = matches!(self.ast.expr(lhs), Expr::Wildcard)
            || matches!(self.ast.expr(rhs), Expr::Wildcard);
        if !is_placeholder {
            self.error(
                self.ast.expr_spans[lhs.0 as usize].clone(),
                "a mem bound must reference each element via `_` (e.g. `where _ < 20`)".to_string(),
            );
        }
    }

    /// v18's own sibling of `check_mem_bound_shape`/`check_ret_bound_
    /// shape` immediately above -- same reasoning: a struct field has no
    /// scoped `DefId` of its own (`DefKind::Struct`'s own doc comment:
    /// field names are "never separately declared... resolved
    /// contextually by types.rs"), so its self-reference position is
    /// checked by SHAPE against `_` too, for the same consistency this
    /// whole retrofit exists for.
    fn check_struct_field_bound_shape(&mut self, bound: ExprId) {
        let Expr::Binary { lhs, rhs, .. } = self.ast.expr(bound).clone() else {
            return;
        };
        // Stage 3's own operator generalization -- see `check_bound_
        // self_reference`'s own doc comment.
        let is_placeholder = matches!(self.ast.expr(lhs), Expr::Wildcard)
            || matches!(self.ast.expr(rhs), Expr::Wildcard);
        if !is_placeholder {
            self.error(
                self.ast.expr_spans[lhs.0 as usize].clone(),
                "a struct field bound must reference the field's own value via `_` (e.g. \
                 `where _ < 20`)"
                    .to_string(),
            );
        }
    }
}
