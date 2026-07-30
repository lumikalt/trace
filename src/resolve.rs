//! Name resolution: builds a definition arena and maps every identifier
//! expression to its definition in an `ExprId -> DefId` side table, per the
//! index-based-AST plan in DESIGN.md.
//!
//! Scoping rules:
//! - Items are mutually recursive within a scope (a rule may call a
//!   function declared after it), so each scope collects declarations
//!   before resolving bodies.
//! - `let` always binds a fresh local and may shadow.
//! - `x := e` binds a fresh local only when `x` does not resolve;
//!   otherwise it is a write to the existing definition. This is what
//!   makes `pc := c` a register write but `a := m[pc]` a local binding.
//! - Names free in a function signature type (`bits[N]`) become implicit
//!   parameters of that function, per DESIGN.md's parameter inference.
//! - `reads`/`writes` arguments must name state (reg/mem/fifo); schedule
//!   directives must name rules.

use crate::ast::{Ast, Effect, Expr, ExprId, FnKind, Item, ItemId, Name, Stmt, StmtId};
use crate::lexer::Span;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DefId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefKind {
    Builtin,
    Module,
    Reg,
    Mem,
    Fifo,
    Input,
    Output,
    /// A `inst name : Module` child instance.
    Inst,
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
                | DefKind::Inst
        )
    }

    pub fn describe(self) -> &'static str {
        match self {
            DefKind::Builtin => "a builtin",
            DefKind::Module => "a module",
            DefKind::Reg => "a register",
            DefKind::Mem => "a memory",
            DefKind::Fifo => "a fifo",
            DefKind::Input => "an input port",
            DefKind::Output => "an output port",
            DefKind::Inst => "a module instance",
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
}

impl Resolution {
    pub fn def(&self, id: DefId) -> &Def {
        &self.defs[id.0 as usize]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveError {
    pub span: Span,
    pub message: String,
}

/// Callable names with no user definition. `sync`/`race` parse as idents;
/// `prio` is a priority encoder; the rest are the primitive vocabulary
/// DESIGN.md examples assume.
const BUILTINS: &[&str] = &[
    "bits", "wire", "list", "any", "clog2", "pack", "trunc", "len", "sync", "race", "prio",
];

pub fn resolve(ast: &Ast) -> (Resolution, Vec<ResolveError>) {
    let mut resolver = Resolver {
        ast,
        res: Resolution::default(),
        errors: Vec::new(),
        scopes: vec![HashMap::new()],
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
        if let Some(prev) = scope.get(&name.text).copied() {
            if !matches!(kind, DefKind::Local | DefKind::ImplicitParam) {
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
        }
        self.scopes
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
        let (name, kind) = match self.ast.item(id) {
            Item::Module { name, .. } => (name.clone(), DefKind::Module),
            Item::Reg { name, .. } => (name.clone(), DefKind::Reg),
            Item::Mem { name, .. } => (name.clone(), DefKind::Mem),
            Item::Fifo { name, .. } => (name.clone(), DefKind::Fifo),
            Item::Input { name, .. } => (name.clone(), DefKind::Input),
            Item::Output { name, .. } => (name.clone(), DefKind::Output),
            Item::Inst { name, .. } => (name.clone(), DefKind::Inst),
            Item::Rule { name, .. } => (name.clone(), DefKind::Rule),
            Item::Fn { name, kind, .. } => {
                let def_kind = match kind {
                    FnKind::Fn => DefKind::Fn,
                    FnKind::Spec => DefKind::Spec,
                    FnKind::Impl { .. } => DefKind::Impl,
                };
                (name.clone(), def_kind)
            }
            Item::Schedule { .. } => return,
        };
        let def = self.declare(&name, kind);
        self.res.item_defs.insert(id, def);
    }

    fn resolve_item(&mut self, id: ItemId) {
        match self.ast.item(id) {
            Item::Module { items, .. } => {
                self.scopes.push(HashMap::new());
                self.resolve_scope(&items.clone());
                self.scopes.pop();
            }
            Item::Reg { ty, init, .. } => {
                self.resolve_expr(*ty, false);
                if let Some(init) = init {
                    self.resolve_expr(*init, false);
                }
            }
            Item::Mem { ty, .. } | Item::Fifo { ty, .. } | Item::Input { ty, .. } => {
                self.resolve_expr(*ty, false);
            }
            Item::Output { ty, init, .. } => {
                self.resolve_expr(*ty, false);
                if let Some(init) = init {
                    self.resolve_expr(*init, false);
                }
            }
            Item::Inst { module, .. } => {
                self.resolve_expr(*module, false);
                if let Some(def) = self.res.expr_defs.get(module).copied()
                    && self.res.def(def).kind != DefKind::Module
                {
                    self.error(
                        self.ast.expr_spans[module.0 as usize].clone(),
                        format!(
                            "`{}` is {}, not a module",
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
                let effects = effects.clone();
                let body = body.clone();
                self.scopes.push(HashMap::new());
                // Signature types first: free names bind as implicit
                // params, so `reqs : bits[N]` introduces `N` before the
                // body resolves.
                for param in &params {
                    self.resolve_expr(param.ty, true);
                    self.declare(&param.name, DefKind::Param);
                }
                if let Some(ret) = ret {
                    self.resolve_expr(ret, true);
                }
                self.check_effect_args(&effects);
                self.resolve_stmts(&body);
                self.scopes.pop();
            }
            Item::Schedule { directives } => {
                let names: Vec<Name> = directives
                    .iter()
                    .flat_map(|d| match d {
                        crate::ast::ScheduleDirective::Urgency(ns) => ns.clone(),
                        crate::ast::ScheduleDirective::ConflictFree(ns) => ns.clone(),
                    })
                    .collect();
                for name in names {
                    match self.lookup(&name.text) {
                        None => self.error(
                            name.span.clone(),
                            format!("cannot find rule `{}`", name.text),
                        ),
                        Some(def) if self.res.def(def).kind != DefKind::Rule => self.error(
                            name.span.clone(),
                            format!(
                                "`{}` is {}, not a rule",
                                name.text,
                                self.res.def(def).kind.describe()
                            ),
                        ),
                        Some(_) => {}
                    }
                }
            }
        }
    }

    /// `reads {a, b}` / `writes {a}` arguments must name state.
    fn check_effect_args(&mut self, effects: &[Effect]) {
        for effect in effects {
            for arg in &effect.args {
                match self.lookup(&arg.text) {
                    None => self.error(
                        arg.span.clone(),
                        format!("cannot find state `{}`", arg.text),
                    ),
                    Some(def) if !self.res.def(def).kind.is_state() => self.error(
                        arg.span.clone(),
                        format!(
                            "`{}` is {}, not state (reg, mem, fifo, input, output, or inst)",
                            arg.text,
                            self.res.def(def).kind.describe()
                        ),
                    ),
                    Some(_) => {}
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
                            if self.res.def(def).kind == DefKind::Input {
                                self.error(
                                    self.ast.expr_spans[lhs.0 as usize].clone(),
                                    format!(
                                        "cannot assign to `{text}`: it is an input port \
                                         (inputs are read-only, driven from outside the module)"
                                    ),
                                );
                            } else if self.res.def(def).kind == DefKind::Inst {
                                self.error(
                                    self.ast.expr_spans[lhs.0 as usize].clone(),
                                    format!(
                                        "cannot assign to `{text}` directly: it is a module \
                                         instance; write a specific port instead (`{text}.port \
                                         := ...`)"
                                    ),
                                );
                            }
                            self.res.expr_defs.insert(lhs, def);
                        } else {
                            let name = Name {
                                text: text.clone(),
                                span: self.ast.expr_spans[lhs.0 as usize].clone(),
                            };
                            let def = self.declare(&name, DefKind::Local);
                            self.res.expr_defs.insert(lhs, def);
                        }
                    }
                    _ => self.resolve_expr(lhs, false),
                }
            }
            Stmt::Let { name, init } => {
                self.resolve_expr(init, false);
                self.declare(&name, DefKind::Local);
            }
            Stmt::Tick => {}
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
            Stmt::While { cond, body } => {
                self.resolve_expr(cond, false);
                self.scopes.push(HashMap::new());
                self.resolve_stmts(&body);
                self.scopes.pop();
            }
        }
    }

    /// `in_type`: name misses bind implicit parameters instead of erroring
    /// (only signature types pass true).
    fn resolve_expr(&mut self, id: ExprId, in_type: bool) {
        match self.ast.expr(id).clone() {
            Expr::Ident(text) => {
                if let Some(def) = self.lookup(&text) {
                    self.res.expr_defs.insert(id, def);
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
            Expr::Int(_) | Expr::Wildcard => {}
            Expr::Unary { operand, .. } => self.resolve_expr(operand, in_type),
            Expr::Binary { lhs, rhs, .. } => {
                self.resolve_expr(lhs, in_type);
                self.resolve_expr(rhs, in_type);
            }
            Expr::Guard(inner) => self.resolve_expr(inner, in_type),
            // Field names are structural; only the base resolves here.
            Expr::Field { base, .. } => self.resolve_expr(base, in_type),
            Expr::Call { callee, args } | Expr::Bracket { callee, args } => {
                self.resolve_expr(callee, in_type);
                for arg in args {
                    self.resolve_expr(arg, in_type);
                }
            }
            Expr::Spawn(inner) => self.resolve_expr(inner, in_type),
        }
    }
}
