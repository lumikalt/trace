//! Type and width checking: DESIGN.md's two solvers, kept separate.
//!
//! Solver 1 (shapes + instantiation): every expression gets a `Ty`. Calls
//! to functions with implicit width parameters (`bits[N]`) instantiate
//! them by matching concrete argument widths — the unification half.
//!
//! Solver 2 (widths): widths are computed with monotone rules (`+`/`-`
//! keep max width, `*` sums, comparisons give 1) and only *checked* where
//! both sides are known. Rebinding a local widens its width; bodies
//! re-type until the local table is stable, with a divergence cap.
//!
//! Generic bodies (widths depending on unsolved implicit params) are
//! shape-checked only; their widths check numerically at each concrete
//! call site after instantiation.
//!
//! Width rules follow Chisel-style modular arithmetic: `a + b` has width
//! `max(|a|,|b|)`, and an integer literal absorbs into the other
//! operand's width (it must fit). Writing a wider value into a narrower
//! register is an error that names `trunc` — no silent truncation.

use crate::ast::{Ast, BinOp, Expr, ExprId, Item, ItemId, Stmt, StmtId, UnOp};
use crate::lexer::Span;
use crate::resolve::{DefId, DefKind, Resolution, is_guard_like};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    Known(u64),
    /// Depends on an unsolved implicit parameter; checked at concrete
    /// call sites, not here.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ty {
    Bits(Width),
    Mem {
        elem: Box<Ty>,
        len: u64,
    },
    Fifo {
        elem: Box<Ty>,
        depth: u64,
    },
    /// A `spawn`'s result: `.result` yields the wrapped type, `.done`
    /// (bits[1]) reports whether the spawned FSM has finished.
    Handle(Box<Ty>),
    /// Elaboration-time integer (literals, `int` params).
    Int,
    /// `list[T]` — elaboration-time only, no runtime representation.
    /// Never carries a length: a `list[T]` param/body is type-checked
    /// generically (like a generic `bits[N]` body), the same length-
    /// agnostic shape check for any call site; the actual element COUNT
    /// only ever matters to the elaboration-time interpreter
    /// (`firrtl/elaborate.rs`), which operates on the real call site's
    /// `Expr::ListLit` directly, never through this type.
    List(Box<Ty>),
    Unit,
    /// A `struct Name { ... }` value. `name` is carried alongside `def`
    /// purely for `Display` (error messages read `Pair`, not a raw
    /// `DefId`) — `def` is what equality/lookup actually key off of;
    /// `struct_fields` (`Types`) holds the declared field list itself.
    Struct {
        def: DefId,
        name: String,
    },
    /// `?T` — sugar for a compiler-synthesized `{ valid: bit, data: T }`
    /// struct (see `firrtl`'s "Struct emission" for the flattening this
    /// shares with an ordinary declared struct). Unlike `Ty::Struct`,
    /// carries no `DefId`: there's no user declaration to key off of,
    /// `T` alone is the identity.
    Option(Box<Ty>),
    /// The literal `false`'s own type — unifies ONLY against a
    /// `Ty::Option` target (`check_assignable`), never a general
    /// `bits[1]`. Kept as its own sentinel (mirroring `Ty::Int`, the
    /// untyped-integer-literal placeholder) rather than reusing
    /// `Ty::Unknown`, which unifies with anything and would let `false`
    /// silently pass as a value of any type at all, not just an absent
    /// `?T`.
    AbsentLit,
    /// `optional <expr>`'s own type — mirrors `AbsentLit`: no standalone
    /// shape of its own, unifies ONLY against a `Ty::Option` target
    /// (`check_assignable`), which supplies the layer `optional` itself
    /// doesn't know. Carries the wrapped expression's `ExprId` (not a
    /// precomputed `Ty`) so unification recurses through `check_
    /// assignable` again on demand, against the TARGET's own inner —
    /// letting `optional (optional e)` peel one target layer per
    /// `optional` regardless of how many the target actually has.
    Optional(ExprId),
    /// Recovery type: unifies with anything, silences cascades.
    Unknown,
}

impl std::fmt::Display for Ty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ty::Bits(Width::Known(w)) => write!(f, "[{w}]"),
            Ty::Bits(Width::Unknown) => write!(f, "[?]"),
            Ty::Mem { elem, len } => write!(f, "{elem}[{len}]"),
            Ty::Fifo { elem, depth } => write!(f, "fifo[{depth}] of {elem}"),
            Ty::Handle(inner) => write!(f, "handle of {inner}"),
            Ty::Int => write!(f, "int"),
            Ty::List(elem) => write!(f, "list[{elem}]"),
            Ty::Unit => write!(f, "unit"),
            Ty::Struct { name, .. } => write!(f, "struct {name}"),
            Ty::Option(inner) => write!(f, "?{inner}"),
            Ty::AbsentLit => write!(f, "false"),
            Ty::Optional(_) => write!(f, "optional"),
            Ty::Unknown => write!(f, "?"),
        }
    }
}

#[derive(Debug, Default)]
pub struct Types {
    pub expr_tys: HashMap<ExprId, Ty>,
    /// Type of every local (`let`/fresh `:=`) once its body's fixed point
    /// is reached. Keyed by `DefId` since locals are declared per rule/fn.
    pub local_tys: HashMap<DefId, Ty>,
    /// Declared type of every reg/mem/fifo, keyed by its `DefId`.
    pub state_tys: HashMap<DefId, Ty>,
    /// Every module's port list: `(port name, Input or Output, type)`.
    /// Keyed by the module's own `DefId`, not just modules that happen to
    /// be instantiated — `inst` may reference any top-level module.
    pub module_ports: HashMap<DefId, Vec<(String, DefKind, Ty)>>,
    /// An `inst` def -> the module `DefId` it instantiates. A separate
    /// side table rather than a `Ty` variant: an instance isn't a scalar
    /// value, only its ports are.
    pub instance_module: HashMap<DefId, DefId>,
    /// A `struct` def -> its declared, ordered field list. Keyed by the
    /// struct's own `DefId`, mirroring `module_ports`.
    pub struct_fields: HashMap<DefId, Vec<(String, Ty)>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeError {
    pub span: Span,
    pub message: String,
}

/// Re-type a body at most this many times waiting for local widths to
/// stabilize; hitting the cap means widths grow without bound.
const WIDEN_CAP: usize = 50;

/// The two synthetic field names every `?T` value has — shared between
/// the `.field`-read arm (`Ty::Option` case, below) and the
/// destructuring exhaustiveness check (`declared_fields`), so a future
/// change to `?T`'s shape only has one array to update.
const OPTION_FIELDS: [&str; 2] = ["valid", "data"];

pub fn check(ast: &Ast, res: &Resolution) -> (Types, Vec<TypeError>) {
    let mut checker = TypeChecker {
        ast,
        res,
        def_items: res.item_defs.iter().map(|(i, d)| (*d, *i)).collect(),
        state_tys: HashMap::new(),
        types: Types::default(),
        errors: Vec::new(),
        emit: false,
    };
    // `collect_structs` first: `collect_state` now type-checks a struct-
    // typed reg/output's own init expression (missing/extra/mistyped
    // fields), which needs `struct_fields` already populated to validate
    // against — found the hard way when it wasn't (missing/extra-field
    // inits silently passed instead of erroring).
    checker.collect_structs();
    checker.collect_state();
    checker.collect_module_ports();
    checker.check_all();
    // Runs last: needs every body's `expr_tys` already populated (see
    // `Destructure::source_field_base`'s own doc comment, ast.rs), and
    // runs exactly once (unlike a rule/fn body's own fixpoint re-typing,
    // there's nothing here to re-stabilize), so there's no risk of the
    // widening loop's usual double-emit problem.
    checker.check_destructures();
    (checker.types, checker.errors)
}

struct TypeChecker<'a> {
    ast: &'a Ast,
    res: &'a Resolution,
    def_items: HashMap<DefId, ItemId>,
    /// Declared type of every reg/mem/fifo def.
    state_tys: HashMap<DefId, Ty>,
    types: Types,
    errors: Vec<TypeError>,
    /// Errors are emitted only on the final, stable typing pass.
    emit: bool,
}

fn bits_needed(v: u64) -> u64 {
    (64 - v.leading_zeros() as u64).max(1)
}

fn clog2(v: u64) -> u64 {
    if v <= 1 {
        0
    } else {
        64 - (v - 1).leading_zeros() as u64
    }
}

impl<'a> TypeChecker<'a> {
    fn error(&mut self, span: Span, message: String) {
        if self.emit {
            self.errors.push(TypeError { span, message });
        }
    }

    fn expr_span(&self, id: ExprId) -> Span {
        self.ast.expr_spans[id.0 as usize].clone()
    }

    // --- declared state types ---

    fn collect_state(&mut self) {
        self.emit = true;
        let mut stack: Vec<ItemId> = self.ast.roots.clone();
        while let Some(id) = stack.pop() {
            let Some(def) = self.res.item_defs.get(&id).copied() else {
                if let Item::Module { items, .. } = self.ast.item(id) {
                    stack.extend(items.iter().copied());
                }
                continue;
            };
            match self.ast.item(id).clone() {
                Item::Module { items, .. } => stack.extend(items),
                Item::Reg { ty, init, .. } => {
                    let ty = self.eval_ty(ty, &HashMap::new());
                    if !matches!(
                        ty,
                        Ty::Bits(_) | Ty::Struct { .. } | Ty::Option(_) | Ty::Unknown
                    ) {
                        let span = self.ast.item_spans[id.0 as usize].clone();
                        self.error(span, format!("a reg holds bits or a struct, not {ty}"));
                    }
                    if let Some(init) = init {
                        if self.contains_struct_update(init) {
                            self.error(
                                self.expr_span(init),
                                "`..` isn't supported in a reg init (v0 restriction): a \
                                 reg's reset value must be fully explicit, not composed \
                                 from an existing value's fields; give every field \
                                 directly instead"
                                    .to_string(),
                            );
                        }
                        // A struct- or Option-typed init is a real
                        // expression tree (missing/extra/mistyped
                        // fields, `false`/coerced-present, not just a
                        // width-fit check) — `check_literal_fits` alone
                        // (which only ever looks at `Ty::Bits` targets)
                        // would silently skip all of that, since nothing
                        // else in `collect_state` ever runs `type_expr`
                        // over a reg/output's own init. Also routed
                        // through here whenever the init is literally
                        // `false` or `optional <e>`, regardless of `ty`
                        // — neither is const-evaluable as an integer
                        // (`Ty::AbsentLit`/`Ty::Optional` are sentinels,
                        // not `Ty::Bits`), so `check_literal_fits` would
                        // otherwise silently skip validating either one
                        // entirely (e.g. `reg x : [8] = optional false`
                        // passing with no error at all — self-caught by
                        // probing exactly that). Every other `Ty::Bits`
                        // init is untouched (still just `check_literal_
                        // fits`, matching every existing example/test).
                        if matches!(ty, Ty::Struct { .. } | Ty::Option(_))
                            || matches!(self.ast.expr(init), Expr::Absent | Expr::Optional(_))
                        {
                            let mut locals = HashMap::new();
                            let init_ty = self.type_expr(init, &mut locals);
                            self.check_assignable(&init_ty, &ty, self.expr_span(init), "reg init");
                        } else {
                            self.check_literal_fits(init, &ty);
                        }
                    }
                    self.state_tys.insert(def, ty);
                }
                Item::Mem { ty, .. } => {
                    let ty = self.eval_ty(ty, &HashMap::new());
                    if !matches!(ty, Ty::Mem { .. } | Ty::Unknown) {
                        let span = self.ast.item_spans[id.0 as usize].clone();
                        self.error(
                            span,
                            format!("a mem needs an element type and a size (`[w][n]`), got {ty}"),
                        );
                    }
                    self.state_tys.insert(def, ty);
                }
                Item::Fifo { ty, .. } => {
                    let (elem, depth) = self.eval_fifo_ty(ty, &HashMap::new());
                    self.state_tys.insert(
                        def,
                        Ty::Fifo {
                            elem: Box::new(elem),
                            depth,
                        },
                    );
                }
                Item::Input { ty, .. } => {
                    let ty = self.eval_ty(ty, &HashMap::new());
                    if !matches!(
                        ty,
                        Ty::Bits(_) | Ty::Struct { .. } | Ty::Option(_) | Ty::Unknown
                    ) {
                        let span = self.ast.item_spans[id.0 as usize].clone();
                        self.error(span, format!("an input holds bits or a struct, not {ty}"));
                    }
                    self.state_tys.insert(def, ty);
                }
                Item::Output { ty, init, .. } => {
                    let ty = self.eval_ty(ty, &HashMap::new());
                    if !matches!(
                        ty,
                        Ty::Bits(_) | Ty::Struct { .. } | Ty::Option(_) | Ty::Unknown
                    ) {
                        let span = self.ast.item_spans[id.0 as usize].clone();
                        self.error(span, format!("an output holds bits or a struct, not {ty}"));
                    }
                    if let Some(init) = init {
                        if self.contains_struct_update(init) {
                            self.error(
                                self.expr_span(init),
                                "`..` isn't supported in an output init (v0 restriction): \
                                 an output's reset value must be fully explicit, not \
                                 composed from an existing value's fields; give every \
                                 field directly instead"
                                    .to_string(),
                            );
                        }
                        if matches!(ty, Ty::Struct { .. } | Ty::Option(_))
                            || matches!(self.ast.expr(init), Expr::Absent | Expr::Optional(_))
                        {
                            let mut locals = HashMap::new();
                            let init_ty = self.type_expr(init, &mut locals);
                            self.check_assignable(
                                &init_ty,
                                &ty,
                                self.expr_span(init),
                                "output init",
                            );
                        } else {
                            self.check_literal_fits(init, &ty);
                        }
                    }
                    self.state_tys.insert(def, ty);
                }
                _ => {}
            }
        }
        self.emit = false;
        self.types.state_tys = self.state_tys.clone();
    }

    /// Every `struct`'s declared, ordered field list — v0: each field
    /// must itself be a plain `bits[N]` (no nested structs; `eval_ty`
    /// would happily resolve a field naming ANOTHER struct, since it
    /// doesn't distinguish "a struct's own field type" from any other
    /// type position, so that has to be rejected explicitly here rather
    /// than falling out of the type system on its own).
    fn collect_structs(&mut self) {
        // Runs before `collect_state` sets this (needed there now too —
        // a struct-typed reg/output init is type-checked against
        // `struct_fields`, so this must populate it first) — without its
        // own `emit = true`, every error below is silently discarded
        // (`Emitter::error`'s own gate), found the hard way when a
        // nested-struct field passed with zero diagnostic at all.
        self.emit = true;
        let mut stack: Vec<ItemId> = self.ast.roots.clone();
        while let Some(id) = stack.pop() {
            if let Item::Module { items, .. } = self.ast.item(id) {
                stack.extend(items.iter().copied());
            }
            let Item::Struct { fields, .. } = self.ast.item(id).clone() else {
                continue;
            };
            let Some(&struct_def) = self.res.item_defs.get(&id) else {
                continue;
            };
            let mut field_tys = Vec::new();
            for field in &fields {
                let ty = self.eval_ty(field.ty, &HashMap::new());
                // A struct field may be `bits[N]`, another struct, or a
                // `?T` (all flattened recursively at emission time, see
                // `firrtl::struct_field_widths`/`option_field_widths`) —
                // anything else (fifo/mem/list/handle) has no flat
                // register shape.
                if !matches!(
                    ty,
                    Ty::Bits(_) | Ty::Struct { .. } | Ty::Option(_) | Ty::Unknown
                ) {
                    self.error(
                        self.ast.expr_spans[field.ty.0 as usize].clone(),
                        format!("a struct field holds bits or a struct, not {ty}"),
                    );
                }
                field_tys.push((field.name.text.clone(), ty));
            }
            self.types.struct_fields.insert(struct_def, field_tys);
        }
        self.check_struct_cycles();
        self.emit = false;
    }

    /// The full field-name list a struct/`?T` type declares, for the
    /// destructuring exhaustiveness check below -- `None` for anything
    /// else (`Ty::Unknown` from an earlier type error, or a plain
    /// `bits[N]`/other value, which the per-item `.field` projection
    /// already rejects on its own; nothing more to say here).
    fn declared_fields(&self, ty: &Ty) -> Option<Vec<String>> {
        match ty {
            Ty::Struct { def, .. } => self
                .types
                .struct_fields
                .get(def)
                .map(|fields| fields.iter().map(|(name, _)| name.clone()).collect()),
            Ty::Option(_) => Some(OPTION_FIELDS.iter().map(|s| s.to_string()).collect()),
            _ => None,
        }
    }

    /// `let {a, c} = s` must either name every field `s`'s type
    /// declares, or end in `..` to explicitly discard the rest (see
    /// `Destructure`, ast.rs) -- mirrors `Expr::StructLit`'s own
    /// missing-field check on the construction side, so both directions
    /// are exhaustive-by-default the same way, `..`/`..base` the
    /// matching opt-out on each. Runs once, at the very end of
    /// `check()`, after every body's `expr_tys` is fully populated.
    fn check_destructures(&mut self) {
        self.emit = true;
        let ast = self.ast;
        for group in &ast.destructures {
            let Some(source_ty) = self.types.expr_tys.get(&group.source_field_base).cloned() else {
                continue;
            };
            let Some(declared) = self.declared_fields(&source_ty) else {
                continue;
            };
            // A named field that isn't real already got its own "no
            // field `x`" error from the per-item `.field` projection --
            // don't also pile on a likely-spurious "missing field(s)"
            // for what's probably just a typo (self-caught: `let
            // {vlaid, data} = p` briefly reported both).
            if !group
                .named_fields
                .iter()
                .all(|f| declared.contains(&f.text))
            {
                continue;
            }
            if group.has_rest {
                continue;
            }
            let missing: Vec<&str> = declared
                .iter()
                .filter(|d| !group.named_fields.iter().any(|f| &f.text == *d))
                .map(|d| d.as_str())
                .collect();
            if !missing.is_empty() {
                self.error(
                    group.span.clone(),
                    format!(
                        "missing field(s): {} -- name them, or add `..` to discard the rest",
                        missing.join(", ")
                    ),
                );
            }
        }
        self.emit = false;
    }

    /// A struct field's declared type may itself be a struct — reject
    /// only the case that would make flattening (recursive by
    /// construction, `firrtl::struct_field_widths`) never terminate: a
    /// struct that directly or transitively contains itself. Runs once
    /// `struct_fields` is fully populated (order-independent: a field's
    /// `Ty::Struct` only needs the referenced struct's `DefId`, not its
    /// own field list, so `collect_structs`'s single top-level walk
    /// above doesn't need to visit structs in dependency order).
    fn check_struct_cycles(&mut self) {
        let defs: Vec<DefId> = self.types.struct_fields.keys().copied().collect();
        for start in defs {
            let mut visited = std::collections::HashSet::new();
            if self.struct_reaches(start, start, &mut visited) {
                let span = self
                    .def_items
                    .get(&start)
                    .map(|item| self.ast.item_spans[item.0 as usize].clone())
                    .unwrap_or(0..0);
                self.error(
                    span,
                    format!(
                        "struct `{}` is recursively defined (directly or indirectly \
                         contains itself)",
                        self.res.def(start).name
                    ),
                );
            }
        }
    }

    fn struct_reaches(
        &self,
        from: DefId,
        target: DefId,
        visited: &mut std::collections::HashSet<DefId>,
    ) -> bool {
        if !visited.insert(from) {
            return false;
        }
        let Some(fields) = self.types.struct_fields.get(&from) else {
            return false;
        };
        for (_, ty) in fields {
            if let Ty::Struct { def, .. } = ty
                && (*def == target || self.struct_reaches(*def, target, visited))
            {
                return true;
            }
        }
        false
    }

    /// Every module's port list (its direct `in`/`out` children) and
    /// every `inst`'s target module, keyed off `state_tys` already built
    /// by `collect_state`. Must run after it.
    fn collect_module_ports(&mut self) {
        let mut stack: Vec<ItemId> = self.ast.roots.clone();
        while let Some(id) = stack.pop() {
            let Item::Module { items, .. } = self.ast.item(id).clone() else {
                continue;
            };
            if let Some(&module_def) = self.res.item_defs.get(&id) {
                let mut ports = Vec::new();
                for item_id in &items {
                    match self.ast.item(*item_id) {
                        Item::Input { .. } | Item::Output { .. } => {
                            if let Some(&def) = self.res.item_defs.get(item_id) {
                                let kind = self.res.def(def).kind;
                                let ty = self.state_tys.get(&def).cloned().unwrap_or(Ty::Unknown);
                                ports.push((self.res.def(def).name.clone(), kind, ty));
                            }
                        }
                        Item::Inst { module, .. } => {
                            if let (Some(&inst_def), Some(&target_def)) = (
                                self.res.item_defs.get(item_id),
                                self.res.expr_defs.get(module),
                            ) {
                                self.types.instance_module.insert(inst_def, target_def);
                            }
                        }
                        _ => {}
                    }
                }
                self.types.module_ports.insert(module_def, ports);
            }
            stack.extend(items.iter().copied());
        }
    }

    /// `base` names a module instance -> the module `DefId` it instantiates.
    fn instance_module_of(&self, base: ExprId) -> Option<DefId> {
        let def = *self.res.expr_defs.get(&base)?;
        self.types.instance_module.get(&def).copied()
    }

    fn find_port(&self, module_def: DefId, name: &str) -> Option<(DefKind, Ty)> {
        self.types
            .module_ports
            .get(&module_def)?
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, k, t)| (*k, t.clone()))
    }

    /// A fifo's type is either a bare element type (`bits[8]`, depth 1)
    /// or `[depth]elem_ty` (parser.rs's `parse_fifo_depth_ty`), which
    /// parses to the exact same `Bracket { callee: elem_ty, args: [depth] }`
    /// shape a mem's postfix `elem_ty[depth]` would produce. Extracts
    /// elem/depth directly from that shape rather than routing through
    /// `eval_ty`'s generic Bracket fallthrough, which would wrap the
    /// result in `Ty::Mem` instead of the flat `(elem, depth)` a fifo
    /// needs.
    fn eval_fifo_ty(&mut self, id: ExprId, env: &HashMap<DefId, u64>) -> (Ty, u64) {
        if let Expr::Bracket { callee, args } = self.ast.expr(id).clone()
            && !self.is_builtin(callee, "bits")
            && !self.is_builtin(callee, "list")
        {
            let elem = self.eval_ty(callee, env);
            return match args.first().and_then(|a| self.const_eval(*a, env)) {
                Some(depth) => (elem, depth),
                None => {
                    self.error(
                        self.expr_span(id),
                        "fifo depth must be an elaboration-time constant".to_string(),
                    );
                    (elem, 1)
                }
            };
        }
        (self.eval_ty(id, env), 1)
    }

    /// Evaluate a type expression. `env` carries solved implicit params.
    fn eval_ty(&mut self, id: ExprId, env: &HashMap<DefId, u64>) -> Ty {
        match self.ast.expr(id).clone() {
            Expr::Bracket { callee, args } => {
                if self.is_builtin(callee, "bits") {
                    if args.len() != 1 {
                        self.error(self.expr_span(id), "`bits` takes one width".to_string());
                        return Ty::Unknown;
                    }
                    return match self.const_eval(args[0], env) {
                        Some(w) => Ty::Bits(Width::Known(w)),
                        None => Ty::Bits(Width::Unknown),
                    };
                }
                if self.is_builtin(callee, "list") {
                    if args.len() != 1 {
                        self.error(
                            self.expr_span(id),
                            "`list` takes one element type, e.g. `list[8]` or `list[Pair]`"
                                .to_string(),
                        );
                        return Ty::Unknown;
                    }
                    return Ty::List(Box::new(self.eval_elem_ty(args[0], env)));
                }
                let elem = self.eval_ty(callee, env);
                if matches!(elem, Ty::Unknown) {
                    return Ty::Unknown;
                }
                let Some(len) = args.first().and_then(|a| self.const_eval(*a, env)) else {
                    self.error(
                        self.expr_span(id),
                        "memory size must be an elaboration-time constant".to_string(),
                    );
                    return Ty::Unknown;
                };
                Ty::Mem {
                    elem: Box::new(elem),
                    len,
                }
            }
            // Tolerate builtin type constructors we do not model yet
            // (wire); recognize a struct name; reject unknown identifiers
            // as types.
            Expr::Ident(name) => {
                let def = self.res.expr_defs.get(&id).copied();
                match def.map(|d| self.res.def(d).kind) {
                    Some(DefKind::Builtin) => Ty::Unknown,
                    Some(DefKind::Struct) => Ty::Struct {
                        def: def.unwrap(),
                        name,
                    },
                    _ => {
                        self.error(self.expr_span(id), "expected a type here".to_string());
                        Ty::Unknown
                    }
                }
            }
            Expr::Call { .. } => Ty::Unknown,
            Expr::OptionTy(inner) => Ty::Option(Box::new(self.eval_ty(inner, env))),
            _ => {
                self.error(self.expr_span(id), "expected a type here".to_string());
                Ty::Unknown
            }
        }
    }

    /// `list[T]`'s own single argument, with one extra sugar `eval_ty`
    /// itself doesn't have: a bare width expression (`list[8]`,
    /// `list[N]`, `list[clog2(N)]`) is shorthand for `list[[N]]` (that
    /// is, `list[bits[N]]`) — sidesteps the double-bracket `list[[N]]`
    /// for the overwhelmingly common case, a list of plain bit-vectors,
    /// while `list[Pair]` (a list of some OTHER type, e.g. a struct)
    /// still spells its element type out in full, since only a
    /// `[N]`-shaped element has anything to abbreviate in the first
    /// place. Told apart the same way `parse_expr`'s own `[N]`-vs-list-
    /// literal split is: by shape, not position — an expression that's
    /// ALREADY type-shaped (a `Bracket`, an `OptionTy`, or an `Ident`
    /// naming a declared struct) evaluates as a type normally; anything
    /// else is treated as the width argument an implicit `[...]` would
    /// have wrapped, mirroring the `bits` arm's own Known/Unknown
    /// `const_eval` fallback above.
    fn eval_elem_ty(&mut self, id: ExprId, env: &HashMap<DefId, u64>) -> Ty {
        let already_a_type = match self.ast.expr(id) {
            Expr::Bracket { .. } | Expr::OptionTy(_) => true,
            Expr::Ident(_) => self
                .res
                .expr_defs
                .get(&id)
                .is_some_and(|d| self.res.def(*d).kind == DefKind::Struct),
            _ => false,
        };
        if already_a_type {
            return self.eval_ty(id, env);
        }
        match self.const_eval(id, env) {
            Some(w) => Ty::Bits(Width::Known(w)),
            None => Ty::Bits(Width::Unknown),
        }
    }

    /// Constant-evaluate an elaboration expression, if possible.
    fn const_eval(&self, id: ExprId, env: &HashMap<DefId, u64>) -> Option<u64> {
        match self.ast.expr(id) {
            Expr::Int(v) => Some(*v),
            Expr::SizedInt { value, .. } => Some(*value),
            Expr::Ident(_) => {
                let def = self.res.expr_defs.get(&id)?;
                env.get(def).copied()
            }
            Expr::Binary { op, lhs, rhs } => {
                let l = self.const_eval(*lhs, env)?;
                let r = self.const_eval(*rhs, env)?;
                match op {
                    BinOp::Add => l.checked_add(r),
                    BinOp::Sub => l.checked_sub(r),
                    BinOp::Mul => l.checked_mul(r),
                    BinOp::Div => l.checked_div(r),
                    BinOp::Rem => l.checked_rem(r),
                    BinOp::Shl => l.checked_shl(r as u32),
                    BinOp::Shr => l.checked_shr(r as u32),
                    _ => None,
                }
            }
            Expr::Call { callee, args } => {
                if self.is_builtin(*callee, "clog2") && args.len() == 1 {
                    Some(clog2(self.const_eval(args[0], env)?))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn is_builtin(&self, id: ExprId, name: &str) -> bool {
        self.res.expr_defs.get(&id).is_some_and(|d| {
            let def = self.res.def(*d);
            def.kind == DefKind::Builtin && def.name == name
        })
    }

    // --- bodies ---

    fn check_all(&mut self) {
        let mut stack: Vec<ItemId> = self.ast.roots.clone();
        while let Some(id) = stack.pop() {
            match self.ast.item(id) {
                Item::Module { items, .. } => stack.extend(items.iter().copied()),
                Item::Rule { .. } | Item::Fn { .. } => self.check_body(id),
                _ => {}
            }
        }
    }

    /// Type one body to a fixed point of the local-width table.
    fn check_body(&mut self, id: ItemId) {
        let (params, ret, body) = match self.ast.item(id).clone() {
            Item::Rule { body, .. } => (Vec::new(), None, body),
            Item::Fn {
                params, ret, body, ..
            } => (params, ret, body),
            _ => return,
        };
        let empty = HashMap::new();
        let mut locals: HashMap<DefId, Ty> = HashMap::new();
        for param in &params {
            let ty = self.eval_ty(param.ty, &empty);
            if let Some(def) = self.res.item_defs.get(&id) {
                // Params were declared in the fn's own scope; find their
                // defs via the expr they bind. Param defs are not exprs,
                // so look them up by name among defs is fragile; instead
                // resolve.rs mapped every ident use. Bind by span match.
                let _ = def;
            }
            // A struct or `?T` fn param IS supported: `calls.rs`'s
            // `compile_struct_field_read` chases a param bound to a
            // reg/output/input, another param/local of the same type,
            // OR a struct/`?T`-returning call, through to that value's
            // own root/return (see its own doc comment, and `compile_
            // field_path_value`'s `Expr::Call` case). An argument
            // that's none of those (an arithmetic expression) still
            // errors at emission time, cleanly, not silently. The
            // RETURN-type side is ALSO supported now, the same way: a
            // struct/`?T`-returning callee's value is decomposed one
            // leaf field at a time by `compile_callee_body_field`,
            // reusing the same per-leaf write-threading a struct
            // literal already gets (see TODO.md for the design story).
            // Param defs: resolve declared them; find by name+span.
            for (i, d) in self.res.defs.iter().enumerate() {
                if d.span == param.name.span {
                    locals.insert(DefId(i as u32), ty.clone());
                    break;
                }
            }
        }
        let ret_ty = ret.map(|r| self.eval_ty(r, &empty));

        for _ in 0..WIDEN_CAP {
            let before = locals.clone();
            for stmt in &body {
                self.type_stmt(*stmt, &mut locals, ret_ty.as_ref());
            }
            if locals == before {
                // Stable: one more pass with errors on.
                self.emit = true;
                for stmt in &body {
                    self.type_stmt(*stmt, &mut locals, ret_ty.as_ref());
                }
                self.emit = false;
                self.types.local_tys.extend(locals);
                return;
            }
        }
        self.emit = true;
        let span = self.ast.item_spans[id.0 as usize].clone();
        self.error(
            span,
            "widths in this body grow without bound; add an explicit `trunc`".to_string(),
        );
        self.emit = false;
    }

    fn type_stmt(&mut self, id: StmtId, locals: &mut HashMap<DefId, Ty>, ret: Option<&Ty>) {
        match self.ast.stmt(id).clone() {
            Stmt::Expr(e) => {
                // A bare expression left unused implicitly guards the
                // rule (`resolve::is_guard_like`) -- explicit `e?`
                // included, since `type_expr(Guard(inner))` already
                // passes through to `inner`'s own type -- so it gets
                // the same bits[1] enforcement an `if`/`while`
                // condition already gets. Anything else bare (a call,
                // fifo op, spawn) keeps its own independent type.
                if is_guard_like(self.ast, self.res, e) {
                    self.check_cond(e, locals);
                } else {
                    self.type_expr(e, locals);
                }
            }
            Stmt::Assign { lhs, rhs } => {
                let rhs_ty = self.type_expr(rhs, locals);
                self.type_write(lhs, rhs_ty, rhs, locals);
            }
            Stmt::Let { name, init } => {
                let ty = self.type_expr(init, locals);
                for (i, d) in self.res.defs.iter().enumerate() {
                    if d.span == name.span {
                        locals.insert(DefId(i as u32), ty);
                        break;
                    }
                }
            }
            Stmt::Tick => {}
            Stmt::Return(Some(e)) => {
                let ty = self.type_expr(e, locals);
                if let Some(ret) = ret {
                    self.check_assignable(&ty, ret, self.expr_span(e), "return value");
                }
            }
            Stmt::Return(None) => {}
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                self.check_cond(cond, locals);
                for s in then_body {
                    self.type_stmt(s, locals, ret);
                }
                for s in else_body.unwrap_or_default() {
                    self.type_stmt(s, locals, ret);
                }
            }
            Stmt::While { cond, body } => {
                self.check_cond(cond, locals);
                for s in body {
                    self.type_stmt(s, locals, ret);
                }
            }
        }
    }

    fn check_cond(&mut self, cond: ExprId, locals: &mut HashMap<DefId, Ty>) {
        let ty = self.type_expr(cond, locals);
        // A bare `opt?` statement (`opt : ?T`) unwraps-or-fails,
        // discarding the unwrapped value -- the same "gate the rule,
        // ignore the value" position a bare fifo-op statement already
        // occupies (`is_guard_like` excludes those from `check_cond`
        // entirely; a Guard-wrapped Option can't be excluded the same
        // way up front, since an ORDINARY `(cond)?` still needs this
        // check) -- so `T`'s own width is irrelevant here, unlike a
        // real condition.
        if let Expr::Guard(inner) = self.ast.expr(cond)
            && matches!(self.types.expr_tys.get(inner), Some(Ty::Option(_)))
        {
            return;
        }
        match ty {
            Ty::Bits(Width::Known(1)) | Ty::Bits(Width::Unknown) | Ty::Unknown | Ty::Int => {}
            other => self.error(
                self.expr_span(cond),
                format!("condition must be [1], got {other} (compare explicitly)"),
            ),
        }
    }

    /// `lhs := rhs`: state writes check width; local (re)binds widen.
    fn type_write(
        &mut self,
        lhs: ExprId,
        rhs_ty: Ty,
        rhs: ExprId,
        locals: &mut HashMap<DefId, Ty>,
    ) {
        match self.ast.expr(lhs).clone() {
            Expr::Ident(_) => {
                let Some(def) = self.res.expr_defs.get(&lhs).copied() else {
                    return;
                };
                if let Some(state) = self.state_tys.get(&def).cloned() {
                    self.check_assignable(&rhs_ty, &state, self.expr_span(rhs), "state write");
                    self.check_literal_fits(rhs, &state);
                    if matches!(state, Ty::Struct { .. })
                        && !matches!(
                            self.ast.expr(rhs),
                            Expr::StructLit { .. } | Expr::Call { .. }
                        )
                    {
                        self.error(
                            self.expr_span(rhs),
                            "a struct-typed write's right-hand side must be a struct \
                             literal (v0 restriction) -- copying one struct value into \
                             another isn't supported yet; construct a fresh literal \
                             instead"
                                .to_string(),
                        );
                    }
                    // Same restriction, `?T`'s own flavor: unlike a
                    // struct, an Option has no literal AST form of its
                    // own to require here (`false`, or any bare
                    // present-coerced value, both compile directly) —
                    // so this checks the RHS's TYPE instead of its AST
                    // shape. The one shape emission genuinely can't
                    // handle is another *same-shaped* `?T`-typed value
                    // (`opt2 := opt1`, both `?bits[8]`): there's no
                    // struct literal to decompose fields from, so it
                    // would otherwise silently leave the register frozen
                    // at its reset value. A DIFFERENTLY-shaped Option on
                    // the right (`??bits[8]` state, `?bits[8]` rhs) is
                    // already a plain type mismatch `check_assignable`
                    // above reports on its own -- checking `state ==
                    // rhs_ty` here (not just "both Option") avoids a
                    // second, misleadingly-worded "copy" error on top of
                    // that one (self-caught while probing `??T`: `oo :=
                    // inner` used to emit both).
                    if matches!(state, Ty::Option(_))
                        && state == rhs_ty
                        && !matches!(self.ast.expr(rhs), Expr::Call { .. })
                    {
                        self.error(
                            self.expr_span(rhs),
                            "a `?T`-typed write's right-hand side must be `false` or a \
                             plain value of the wrapped type (v0 restriction) -- copying \
                             one `?T` value into another isn't supported yet"
                                .to_string(),
                        );
                    }
                } else if self.res.def(def).kind == DefKind::Local {
                    let merged = match locals.get(&def) {
                        Some(old) => self.widen(old.clone(), rhs_ty, lhs),
                        None => rhs_ty,
                    };
                    locals.insert(def, merged);
                }
            }
            Expr::Bracket { callee, args } => {
                let base = self.type_expr(callee, locals);
                for a in &args {
                    self.type_expr(*a, locals);
                }
                match base {
                    Ty::Mem { elem, .. } => {
                        self.check_assignable(&rhs_ty, &elem, self.expr_span(rhs), "memory write");
                    }
                    Ty::Unknown => {}
                    other => self.error(
                        self.expr_span(lhs),
                        format!("cannot index-assign into {other}"),
                    ),
                }
            }
            Expr::Field { base, name } => {
                if let Some(module_def) = self.instance_module_of(base) {
                    // `base` is a valid instance reference in this
                    // `.port` position, not a bare value use — do not
                    // route through the generic Ident type check, which
                    // rejects a standalone instance reference.
                    self.types.expr_tys.insert(base, Ty::Unknown);
                    match self.find_port(module_def, &name) {
                        Some((DefKind::Input, port_ty)) => {
                            self.check_assignable(
                                &rhs_ty,
                                &port_ty,
                                self.expr_span(rhs),
                                "instance port write",
                            );
                            self.check_literal_fits(rhs, &port_ty);
                        }
                        Some((_, _)) => self.error(
                            self.expr_span(lhs),
                            format!(
                                "cannot write `{name}`: it is an output port on this \
                                 instance (only input ports can be written)"
                            ),
                        ),
                        None => self.error(
                            self.expr_span(lhs),
                            format!("this instance has no port `{name}`"),
                        ),
                    }
                } else {
                    // Routes through the same read-side logic as any other
                    // field access (rejects a bogus field/base the same
                    // way `x.foo` would as an expression); a handle's or
                    // struct's fields additionally aren't writable at all
                    // (v0 restriction for structs: whole-value assignment
                    // only, `p := Pair{...}` — mirrors `Ty::Handle`'s own
                    // existing read-only restriction, same reasoning: no
                    // answer yet for what a partial-field write does to
                    // the OTHER fields of an if/else-nested assignment).
                    self.type_expr(lhs, locals);
                    match self.types.expr_tys.get(&base) {
                        Some(Ty::Handle(_)) => {
                            self.error(
                                self.expr_span(lhs),
                                format!("cannot write `.{name}`: a handle's fields are read-only"),
                            );
                        }
                        Some(Ty::Struct { .. }) => {
                            self.error(
                                self.expr_span(lhs),
                                format!(
                                    "cannot write `.{name}`: a struct's fields are read-only \
                                     (v0 restriction) — assign the whole value instead"
                                ),
                            );
                        }
                        Some(Ty::Option(_)) => {
                            self.error(
                                self.expr_span(lhs),
                                format!(
                                    "cannot write `.{name}`: a `?T` value's fields are \
                                     read-only — write the whole value instead (`false`, \
                                     or a plain value of the wrapped type)"
                                ),
                            );
                        }
                        _ => {}
                    }
                }
            }
            _ => {
                self.type_expr(lhs, locals);
            }
        }
    }

    /// Widening for rebound locals: same shape, width grows to max.
    fn widen(&mut self, old: Ty, new: Ty, at: ExprId) -> Ty {
        match (&old, &new) {
            (Ty::Unknown, _) => new,
            (_, Ty::Unknown) => old,
            (Ty::Bits(a), Ty::Bits(b)) => match (a, b) {
                (Width::Known(x), Width::Known(y)) => Ty::Bits(Width::Known(*x.max(y))),
                _ => Ty::Bits(Width::Unknown),
            },
            (Ty::Int, Ty::Int) => Ty::Int,
            (Ty::Int, Ty::Bits(_)) => new,
            (Ty::Bits(_), Ty::Int) => old,
            _ if old == new => old,
            _ => {
                self.error(
                    self.expr_span(at),
                    format!("rebinding changes type from {old} to {new}"),
                );
                new
            }
        }
    }

    /// Whether `id` reaches a `Pair{ ..base }`-shaped struct literal
    /// anywhere in its own tree — used to reject `..` in a reg/output
    /// INIT (a compile-TIME constant position, see `option_lit_field_
    /// const`/`struct_lit_field_const`, firrtl/mod.rs), where `base`
    /// would need to be a compile-time constant itself and generally
    /// isn't (a reg reference's flat fields aren't known until runtime).
    /// A walk, not a top-level-only check: `Outer{ inner: Inner{ ..old },
    /// x: 1 }` nests a `..` inside an explicitly-given field's own
    /// literal, still unreachable from a const-eval. Left unrejected,
    /// `struct_lit_field_const`'s existing `fields.iter().find(...)?`
    /// would return `None` for every field `..base` was meant to supply
    /// — routed by its caller (`module.rs`) through `.unwrap_or(0)`, a
    /// silent zero reset with no error at all, the exact same class of
    /// bug `optional opt1`'s aliasing rejection closed for `?T`.
    fn contains_struct_update(&self, id: ExprId) -> bool {
        if let Expr::StructLit { base: Some(_), .. } = self.ast.expr(id) {
            return true;
        }
        crate::lower::sub_exprs(self.ast, id)
            .into_iter()
            .any(|child| self.contains_struct_update(child))
    }

    /// A constant written into `[w]` must fit in `w` bits.
    fn check_literal_fits(&mut self, value: ExprId, target: &Ty) {
        if let Ty::Bits(Width::Known(w)) = target
            && let Some(v) = self.const_eval(value, &HashMap::new())
            && bits_needed(v) > *w
        {
            self.error(self.expr_span(value), format!("{v} does not fit in [{w}]"));
        }
    }

    /// May `value` be written where `target` is expected? Shapes must
    /// match; a known-wider value needs an explicit `trunc`.
    fn check_assignable(&mut self, value: &Ty, target: &Ty, span: Span, what: &str) {
        match (value, target) {
            (Ty::Unknown, _) | (_, Ty::Unknown) => {}
            (Ty::Int, Ty::Bits(_)) => {} // literal absorbs; range-checked at coercion
            (Ty::Bits(wv), Ty::Bits(wt)) => {
                if let (Width::Known(v), Width::Known(t)) = (wv, wt)
                    && v > t
                {
                    self.error(
                        span,
                        format!(
                            "{what} would silently truncate [{v}] to [{t}]; \
                             use `trunc(value, {t})`"
                        ),
                    );
                }
            }
            // `false` constructs the absent value of any `?T`.
            (Ty::AbsentLit, Ty::Option(_)) => {}
            // `optional e` forces exactly the NEXT layer's `valid` to
            // true, then recurses on `e`'s own type (looked up now that
            // it's needed, not precomputed — see `Ty::Optional`'s doc
            // comment) against the TARGET's inner. Checked ahead of the
            // generic bare-value-coercion arm below, whose guard would
            // otherwise accept a `Ty::Optional` value too (it isn't a
            // `Ty::Option`) and keep re-checking the SAME sentinel
            // against successively peeled targets instead of ever
            // unwrapping to `e`.
            (Ty::Optional(inner), Ty::Option(t_inner)) => {
                let inner_ty = self
                    .types
                    .expr_tys
                    .get(inner)
                    .cloned()
                    .unwrap_or(Ty::Unknown);
                // `optional <alias>`, `<alias>` a plain reference already
                // typed `?T` (a reg/param/local/field, not a fresh
                // literal/computed value) — the exact "copy one `?T`
                // value into another" v0 restriction below, one layer up.
                // Emission (`compile_field_path_value`, writes.rs) has no
                // way to thread an ALIASED `?T`'s own live valid/data
                // pair into another Option's flat fields; without this
                // check it silently falls through to that fn's existing
                // aliasing guard, which returns `None` for a value that
                // nothing escalates to a diagnostic on the WRITE side
                // (unlike the read side's `compile_struct_field_read`) —
                // the register would silently hold its previous value
                // instead of tracking `alias`. A CALL returning `?T` is
                // exempt: its return decomposes per-leaf
                // (`compile_call_field_value`) rather than aliasing a
                // flat register, the same exemption `type_write`'s own
                // Option-to-Option rejection already carves out.
                if matches!(inner_ty, Ty::Option(_))
                    && !matches!(self.ast.expr(*inner), Expr::Call { .. })
                {
                    self.error(
                        span,
                        format!(
                            "{what}: `optional` cannot wrap an existing `?T` value directly \
                             (v0 restriction) -- copying one `?T` value into another isn't \
                             supported yet; use a fresh value, `false`, or a nested `optional` \
                             instead"
                        ),
                    );
                    return;
                }
                self.check_assignable(&inner_ty, t_inner, span, what);
            }
            // A bare `T`-shaped value implicitly wraps into `?T` present
            // — reached anywhere `check_assignable` already runs (state
            // writes/inits, return values, call arguments, memory/
            // instance-port writes, struct-literal fields, ...), not
            // hand-restricted to a narrower set of positions. Recurses
            // rather than requiring exact equality, so a too-wide
            // literal into `?bits[8]` still gets the ordinary `trunc`
            // guidance instead of a generic mismatch. Guarded off
            // `value` itself being `Ty::Option` so writing one Option
            // value into another (`opt2 := opt1`) is NOT silently
            // treated as "present, holding an Option" — it falls
            // through to ordinary equality below instead, matching
            // `Ty::Struct`'s own copy-between-two-values handling.
            (_, Ty::Option(inner)) if !matches!(value, Ty::Option(_)) => {
                self.check_assignable(value, inner, span, what);
            }
            _ if value == target => {}
            _ => self.error(span, format!("{what}: expected {target}, got {value}")),
        }
    }

    fn type_expr(&mut self, id: ExprId, locals: &mut HashMap<DefId, Ty>) -> Ty {
        let ty = self.type_expr_inner(id, locals);
        self.types.expr_tys.insert(id, ty.clone());
        ty
    }

    fn type_expr_inner(&mut self, id: ExprId, locals: &mut HashMap<DefId, Ty>) -> Ty {
        match self.ast.expr(id).clone() {
            Expr::Int(_) => Ty::Int,
            // Unlike a bare `Int`, a sized literal has its own definite
            // width, so it types directly as `Bits(Known(width))` — no
            // "absorb from context" — and is range-checked right here,
            // against ITS OWN declared width, rather than deferred to
            // `check_literal_fits` at whatever coercion site it's later
            // used in (matches the same `bits_needed` helper that uses).
            Expr::SizedInt { width, value } => {
                if bits_needed(value) > width {
                    self.error(
                        self.expr_span(id),
                        format!("{value} does not fit in [{width}]"),
                    );
                }
                Ty::Bits(Width::Known(width))
            }
            Expr::Wildcard => Ty::Unknown,
            Expr::Ident(_) => {
                let Some(def) = self.res.expr_defs.get(&id).copied() else {
                    return Ty::Unknown;
                };
                // Bare state idents carry their state type; indexing and
                // fifo ops peel Mem/Fifo wrappers at the use site.
                if let Some(state) = self.state_tys.get(&def) {
                    return state.clone();
                }
                if let Some(local) = locals.get(&def) {
                    return local.clone();
                }
                match self.res.def(def).kind {
                    DefKind::ImplicitParam => Ty::Int,
                    DefKind::Inst => {
                        self.error(
                            self.expr_span(id),
                            format!(
                                "cannot use instance `{}` as a value; access one of its \
                                 ports (`{}.port`)",
                                self.res.def(def).name,
                                self.res.def(def).name
                            ),
                        );
                        Ty::Unknown
                    }
                    _ => Ty::Unknown,
                }
            }
            Expr::Unary { op, operand } => {
                let t = self.type_expr(operand, locals);
                match t {
                    Ty::Bits(_) | Ty::Int | Ty::Unknown => {}
                    other => {
                        self.error(
                            self.expr_span(id),
                            format!("unary operator needs bits, got {other}"),
                        );
                        return Ty::Unknown;
                    }
                }
                // `not` is a real, distinct operator from `~`, not pure
                // sugar for it: both compile to the identical FIRRTL
                // `not` primop (see firrtl/expr.rs), but `not` additionally
                // requires its operand already be `bits[1]` — a
                // guardrail against accidentally bitwise-negating a
                // wider value (`not x` on a `bits[8]` almost certainly
                // means "did you mean a comparison, or `~`?", not "flip
                // every bit"), since `check_cond` below already requires
                // every condition position to be exactly `bits[1]`
                // anyway — there is no implicit "nonzero is true"
                // coercion anywhere in this language for `not` to
                // usefully mean something wider.
                if op == UnOp::Not
                    && !matches!(
                        t,
                        Ty::Bits(Width::Known(1))
                            | Ty::Bits(Width::Unknown)
                            | Ty::Unknown
                            | Ty::Int
                    )
                {
                    self.error(
                        self.expr_span(id),
                        format!(
                            "`not` needs a [1] operand, got {t}; use `~` for a \
                             bitwise complement of a wider value, or compare \
                             explicitly"
                        ),
                    );
                    return Ty::Unknown;
                }
                t
            }
            Expr::Binary { op, lhs, rhs } => {
                let l = self.type_expr(lhs, locals);
                let r = self.type_expr(rhs, locals);
                self.type_binop(op, l, r, id)
            }
            // `(cond)?` ordinarily just passes `inner`'s own type
            // through (its "must be bits[1]" side is `check_cond`'s
            // job, not this fn's). `opt?`, `opt : ?T`, is different: the
            // WHOLE point is unwrapping, so the guard's own type becomes
            // `T`, not `?T` — generalizing `?` from "fails unless
            // bits[1]-true" to "fails unless present, yielding T",
            // reusing the exact same `fails`-folding machinery `f.Deq[]`
            // already has (see `effects.rs`'s `Expr::Guard` handling).
            Expr::Guard(inner) => match self.type_expr(inner, locals) {
                Ty::Option(t) => *t,
                other => other,
            },
            Expr::Field { base, name } => {
                if let Some(module_def) = self.instance_module_of(base) {
                    self.types.expr_tys.insert(base, Ty::Unknown);
                    match self.find_port(module_def, &name) {
                        Some((DefKind::Output, port_ty)) => port_ty,
                        Some((_, _)) => {
                            self.error(
                                self.expr_span(id),
                                format!(
                                    "cannot read `{name}`: it is an input port on this \
                                     instance (only output ports can be read)"
                                ),
                            );
                            Ty::Unknown
                        }
                        None => {
                            self.error(
                                self.expr_span(id),
                                format!("this instance has no port `{name}`"),
                            );
                            Ty::Unknown
                        }
                    }
                } else {
                    match self.type_expr(base, locals) {
                        Ty::Handle(inner) => match name.as_str() {
                            "result" => *inner,
                            "done" => Ty::Bits(Width::Known(1)),
                            _ => {
                                self.error(
                                    self.expr_span(id),
                                    format!(
                                        "a handle has no field `{name}`; only `.result` and \
                                         `.done` are readable"
                                    ),
                                );
                                Ty::Unknown
                            }
                        },
                        Ty::Struct { def, name: sname } => {
                            let fields = self.types.struct_fields.get(&def).cloned();
                            match fields
                                .as_ref()
                                .and_then(|fs| fs.iter().find(|(fname, _)| fname == &name))
                            {
                                Some((_, fty)) => fty.clone(),
                                None => {
                                    self.error(
                                        self.expr_span(id),
                                        format!("struct `{sname}` has no field `{name}`"),
                                    );
                                    Ty::Unknown
                                }
                            }
                        }
                        // `?T`'s two synthetic fields, same escape hatch
                        // a `spawn` handle's `.result`/`.done` already
                        // has: `.valid`/`.data` read WITHOUT unwrap-or-
                        // fail (`opt?`'s job) — the non-failing
                        // alternative `if opt.valid { ...opt.data... }
                        // else { ... }` gives.
                        Ty::Option(inner) => {
                            if !OPTION_FIELDS.contains(&name.as_str()) {
                                self.error(
                                    self.expr_span(id),
                                    format!(
                                        "a `?T` value only has `.valid`/`.data` fields, \
                                         not `.{name}`"
                                    ),
                                );
                                Ty::Unknown
                            } else if name == "valid" {
                                Ty::Bits(Width::Known(1))
                            } else {
                                *inner
                            }
                        }
                        Ty::Unknown => Ty::Unknown,
                        other => {
                            self.error(
                                self.expr_span(id),
                                format!("cannot access field `{name}` on {other}"),
                            );
                            Ty::Unknown
                        }
                    }
                }
            }
            Expr::Spawn(inner) => {
                let ret = self.type_expr(inner, locals);
                Ty::Handle(Box::new(ret))
            }
            Expr::Bracket { callee, args } => self.type_bracket(id, callee, &args, locals),
            Expr::Call { callee, args } => self.type_call(id, callee, &args, locals),
            Expr::ListLit(items) => {
                let Some((&first, rest)) = items.split_first() else {
                    self.error(
                        self.expr_span(id),
                        "a list literal cannot be empty (its element type would be \
                         unknowable)"
                            .to_string(),
                    );
                    return Ty::Unknown;
                };
                let elem = self.type_expr(first, locals);
                for &item in rest {
                    let t = self.type_expr(item, locals);
                    self.check_assignable(&t, &elem, self.expr_span(item), "list element");
                }
                Ty::List(Box::new(elem))
            }
            // Only meaningful as a list-slice `Bracket` argument
            // (`type_bracket` re-matches the raw AST shape there for the
            // real element-type/slice rule); reached here only via the
            // generic per-subexpression walk `type_bracket` already does
            // first, or if used somewhere illegal — `Unknown`, the same
            // treatment the existing two-sided `BinOp::Range` gets in
            // `type_binop` when it shows up outside a bracket.
            Expr::Range { lo, hi } => {
                if let Some(lo) = lo {
                    self.type_expr(lo, locals);
                }
                if let Some(hi) = hi {
                    self.type_expr(hi, locals);
                }
                Ty::Unknown
            }
            // `A or B or C` types like `ListLit`'s element unification:
            // every alternative (a fifo op's element type, or a plain
            // default value) must agree with the first's type. Shape
            // rules (which alts must be fifo ops) are firrtl/checks.rs's
            // job, same division as everywhere else in this module.
            Expr::Or(alts) => {
                let Some((&first, rest)) = alts.split_first() else {
                    self.error(
                        self.expr_span(id),
                        "`or` needs at least two alternatives, e.g. `a.Deq[] or b.Deq[]`"
                            .to_string(),
                    );
                    return Ty::Unknown;
                };
                let elem = self.type_expr(first, locals);
                for &alt in rest {
                    let t = self.type_expr(alt, locals);
                    self.check_assignable(&t, &elem, self.expr_span(alt), "`or` alternative");
                }
                elem
            }
            // `Name { field: expr, ..., ..base }` — v0 requires either an
            // EXHAUSTIVE, one-shot field list (matching Rust's own
            // struct-literal rule: no defaults for a field this literal
            // doesn't mention) or a trailing `..base` supplying every
            // field this literal DOESN'T name — never both partially:
            // `base` fills whatever's absent from THIS literal's own
            // list, it does not recurse into a nested struct/Option
            // field that's itself only partially given. A duplicate
            // field is almost certainly a typo, not a deliberate "last
            // one wins" overwrite. A bad struct NAME (unresolved, or
            // resolved to something that isn't `DefKind::Struct`) is
            // already reported by resolve.rs — this stays defensive
            // (`Ty::Unknown`, no second error) rather than re-checking
            // the same thing.
            Expr::StructLit { name, fields, base } => {
                let Some(&struct_def) = self.res.expr_defs.get(&name) else {
                    for (_, value) in &fields {
                        self.type_expr(*value, locals);
                    }
                    if let Some(base) = base {
                        self.type_expr(base, locals);
                    }
                    return Ty::Unknown;
                };
                let struct_name = match self.ast.expr(name) {
                    Expr::Ident(n) => n.clone(),
                    _ => String::new(),
                };
                let Some(declared) = self.types.struct_fields.get(&struct_def).cloned() else {
                    for (_, value) in &fields {
                        self.type_expr(*value, locals);
                    }
                    if let Some(base) = base {
                        self.type_expr(base, locals);
                    }
                    return Ty::Unknown;
                };
                let mut seen: HashMap<String, ExprId> = HashMap::new();
                for (fname, value) in &fields {
                    let vty = self.type_expr(*value, locals);
                    if seen.contains_key(fname) {
                        self.error(
                            self.expr_span(*value),
                            format!("field `{fname}` is given more than once"),
                        );
                        continue;
                    }
                    seen.insert(fname.clone(), *value);
                    match declared.iter().find(|(dname, _)| dname == fname) {
                        Some((_, dty)) => {
                            self.check_assignable(
                                &vty,
                                dty,
                                self.expr_span(*value),
                                "struct field",
                            );
                            self.check_literal_fits(*value, dty);
                        }
                        None => {
                            self.error(
                                self.expr_span(*value),
                                format!("struct `{struct_name}` has no field `{fname}`"),
                            );
                        }
                    }
                }
                let result = Ty::Struct {
                    def: struct_def,
                    name: struct_name.clone(),
                };
                match base {
                    Some(base) => {
                        let base_ty = self.type_expr(base, locals);
                        self.check_assignable(&base_ty, &result, self.expr_span(base), "`..` base");
                    }
                    None => {
                        let missing: Vec<&str> = declared
                            .iter()
                            .map(|(dname, _)| dname.as_str())
                            .filter(|dname| !seen.contains_key(*dname))
                            .collect();
                        if !missing.is_empty() {
                            self.error(
                                self.expr_span(id),
                                format!(
                                    "struct `{struct_name}` literal is missing field(s): {} \
                                     -- give them explicitly, or add `..base`",
                                    missing.join(", ")
                                ),
                            );
                        }
                    }
                }
                result
            }
            // `?T` has no meaning as a VALUE expression, only a type
            // (`eval_ty`'s own `Expr::OptionTy` arm handles it there) —
            // reachable here only if `?T` shows up somewhere that isn't
            // actually a type position (e.g. `x := ?bits[8]`).
            Expr::OptionTy(_) => {
                self.error(
                    self.expr_span(id),
                    "`?T` is a type, not a value".to_string(),
                );
                Ty::Unknown
            }
            Expr::Absent => Ty::AbsentLit,
            // Type-checks `inner` for its own sake (effects, diagnostics,
            // populating `expr_tys` for `check_assignable`'s later
            // lookup) but deliberately does NOT wrap `inner`'s `Ty` here
            // — see `Ty::Optional`'s own doc comment for why unification
            // needs to stay lazy, recursing through `check_assignable`
            // against the eventual TARGET's inner instead of a type this
            // arm precomputed.
            Expr::Optional(inner) => {
                self.type_expr(inner, locals);
                Ty::Optional(inner)
            }
        }
    }

    /// Solver-2 width rules. Modular arithmetic: `+`/`-`/bitwise keep the
    /// max width; `*` sums; shifts keep the left width; comparisons give
    /// bits[1]. `Int` absorbs into the other side.
    fn type_binop(&mut self, op: BinOp, l: Ty, r: Ty, at: ExprId) -> Ty {
        use BinOp::*;
        let span = self.expr_span(at);
        if matches!(op, Eq | Ne | Lt | Le | Gt | Ge) {
            return Ty::Bits(Width::Known(1));
        }
        if matches!(op, Range | PlusColon | MinusColon) {
            // Only meaningful as a `Bracket`'s own argument (`type_bracket`
            // re-matches the raw AST shape there for the real width rule);
            // reached here only via the generic per-subexpression walk
            // `type_bracket` already does before that re-match, or if
            // used somewhere illegal (e.g. a bare `x := a +: b` statement)
            // — silently `Unknown` either way, same treatment `Range` has
            // always had. Emission's own generic `compile_binop` catch-all
            // still rejects an illegal bare use explicitly, so nothing
            // silently miscompiles.
            return Ty::Unknown;
        }
        match (l, r) {
            (Ty::Unknown, _) | (_, Ty::Unknown) => Ty::Unknown,
            (Ty::Int, Ty::Int) => Ty::Int,
            (Ty::Bits(w), Ty::Int) | (Ty::Int, Ty::Bits(w)) => Ty::Bits(w),
            (Ty::Bits(a), Ty::Bits(b)) => match op {
                Mul => Ty::Bits(match (a, b) {
                    (Width::Known(x), Width::Known(y)) => Width::Known(x + y),
                    _ => Width::Unknown,
                }),
                Shl | Shr | AShr => Ty::Bits(a),
                _ => Ty::Bits(match (a, b) {
                    (Width::Known(x), Width::Known(y)) => Width::Known(x.max(y)),
                    _ => Width::Unknown,
                }),
            },
            (l, r) => {
                self.error(
                    span,
                    format!("operator needs bits operands, got {l} and {r}"),
                );
                Ty::Unknown
            }
        }
    }

    /// Brackets: memory read, bit/slice select, or fifo op.
    fn type_bracket(
        &mut self,
        id: ExprId,
        callee: ExprId,
        args: &[ExprId],
        locals: &mut HashMap<DefId, Ty>,
    ) -> Ty {
        // Fifo op: `f.Deq[]` / `f.Enq[x]`.
        if let Expr::Field { base, name } = self.ast.expr(callee).clone()
            && let Some(def) = self.res.expr_defs.get(&base).copied()
            && let Some(Ty::Fifo { elem, depth }) = self.state_tys.get(&def).cloned()
        {
            self.types.expr_tys.insert(
                callee,
                Ty::Fifo {
                    elem: elem.clone(),
                    depth,
                },
            );
            return match name.as_str() {
                "Deq" => {
                    if !args.is_empty() {
                        self.error(self.expr_span(id), "`Deq[]` takes no arguments".to_string());
                    }
                    *elem
                }
                "Enq" => {
                    if args.len() != 1 {
                        self.error(
                            self.expr_span(id),
                            "`Enq[x]` takes one argument".to_string(),
                        );
                        return Ty::Unit;
                    }
                    let arg = self.type_expr(args[0], locals);
                    self.check_assignable(&arg, &elem, self.expr_span(args[0]), "enqueue");
                    Ty::Unit
                }
                other => {
                    self.error(
                        self.expr_span(id),
                        format!("unknown fifo operation `{other}` (Deq or Enq)"),
                    );
                    Ty::Unknown
                }
            };
        }

        // A builtin used with brackets (`sync[h1, h2]`, `race[h1, h2]`) —
        // brackets mark fallibility, matching `f.Deq[]`/`f.Enq[x]`, so a
        // fallible builtin is called this way rather than with parens.
        // Dispatches through the same per-builtin type rule an ordinary
        // (infallible) `name(args)` call already uses.
        if let Some(def) = self.res.expr_defs.get(&callee).copied()
            && self.res.def(def).kind == DefKind::Builtin
        {
            let name = self.res.def(def).name.clone();
            let arg_tys: Vec<Ty> = args.iter().map(|a| self.type_expr(*a, locals)).collect();
            return self.type_builtin_call(id, &name, args, &arg_tys);
        }

        let base = self.type_expr(callee, locals);
        for a in args {
            self.type_expr(*a, locals);
        }
        match base {
            Ty::Mem { elem, .. } => {
                if args.len() != 1 {
                    self.error(
                        self.expr_span(id),
                        "memory read takes one index".to_string(),
                    );
                }
                *elem
            }
            Ty::Bits(w) => {
                let _ = w;
                // Bit select (`x[i]`), slice (`x[hi..lo]`), or indexed
                // part-select (`x[base +: width]`/`x[base -: width]`) —
                // three shapes, each with a genuinely different width
                // rule, distinguished by the argument's own AST shape.
                if let Some(&arg) = args.first()
                    && let Expr::Binary { op, lhs, rhs } = self.ast.expr(arg).clone()
                {
                    match op {
                        // A slice's width can ONLY be known if BOTH
                        // bounds are compile-time constants — FIRRTL's
                        // `bits` primop needs static bounds, and there's
                        // no way to express a dynamically-SIZED result
                        // in this language's type system at all (unlike
                        // a single index, whose width is always exactly
                        // 1 regardless of whether it's dynamic). A
                        // non-const bound here used to silently fall
                        // through to the `Ty::Bits(Width::Known(1))`
                        // fallback below — a real latent mistyping bug
                        // (a narrower-than-declared value passes
                        // `check_assignable` without complaint) — now an
                        // explicit error instead.
                        BinOp::Range => {
                            return match (
                                self.const_eval(lhs, &HashMap::new()),
                                self.const_eval(rhs, &HashMap::new()),
                            ) {
                                (Some(hi), Some(lo)) => Ty::Bits(Width::Known(hi.abs_diff(lo) + 1)),
                                _ => {
                                    self.error(
                                        self.expr_span(id),
                                        "a slice's bounds (`x[hi..lo]`) must both be \
                                         compile-time constants (this language has no \
                                         way to express a dynamically-sized result); \
                                         use indexed part-select for a dynamic start \
                                         with a fixed width instead — `x[base +: \
                                         width]`/`x[base -: width]`"
                                            .to_string(),
                                    );
                                    Ty::Unknown
                                }
                            };
                        }
                        // Indexed part-select: `base` may be anything —
                        // its own width is irrelevant to the RESULT's
                        // width, which is fixed entirely by `width`, so
                        // (unlike a slice) this only ever needs ONE side
                        // to be a compile-time constant.
                        BinOp::PlusColon | BinOp::MinusColon => {
                            return match self.const_eval(rhs, &HashMap::new()) {
                                Some(width) if width >= 1 => Ty::Bits(Width::Known(width)),
                                Some(_) => {
                                    self.error(
                                        self.expr_span(id),
                                        format!(
                                            "indexed part-select (`{}`) width must be \
                                             at least 1",
                                            op.symbol()
                                        ),
                                    );
                                    Ty::Unknown
                                }
                                None => {
                                    self.error(
                                        self.expr_span(id),
                                        format!(
                                            "indexed part-select (`{}`) needs a \
                                             compile-time constant width",
                                            op.symbol()
                                        ),
                                    );
                                    Ty::Unknown
                                }
                            };
                        }
                        _ => {}
                    }
                }
                // A single index — `x[i]` — is always exactly 1 bit,
                // whether `i` is a compile-time constant or a genuine
                // runtime value; unlike a slice, there's no ambiguity
                // about the result's width to resolve either way.
                Ty::Bits(Width::Known(1))
            }
            // `xs[i]` (an element) or `xs[..mid]`/`xs[mid..]` (a
            // sub-list, `Expr::Range` — the one-sided form, exclusively
            // used for list slicing, never `BinOp::Range`'s two-sided
            // bit-slice shape). Both bounds/the index are checked for
            // being elaboration-time constants by the interpreter
            // (`firrtl/elaborate.rs`) once a real call site exists, not
            // here — this pass only needs the SHAPE, since a `list[T]`
            // body is checked once, generically, independent of any
            // call site's actual length (exactly like a generic
            // `bits[N]` body).
            Ty::List(elem) => {
                if args.len() != 1 {
                    self.error(
                        self.expr_span(id),
                        "list index takes one argument".to_string(),
                    );
                    return Ty::Unknown;
                }
                if matches!(self.ast.expr(args[0]), Expr::Range { .. }) {
                    Ty::List(elem)
                } else {
                    *elem
                }
            }
            Ty::Unknown => Ty::Unknown,
            other => {
                self.error(self.expr_span(id), format!("cannot index {other}"));
                Ty::Unknown
            }
        }
    }

    /// Solver-1 instantiation: builtins by signature; user fns match
    /// implicit width params against concrete argument widths, then the
    /// return type evaluates under that solution.
    fn type_call(
        &mut self,
        id: ExprId,
        callee: ExprId,
        args: &[ExprId],
        locals: &mut HashMap<DefId, Ty>,
    ) -> Ty {
        let arg_tys: Vec<Ty> = args.iter().map(|a| self.type_expr(*a, locals)).collect();
        let Some(def) = self.res.expr_defs.get(&callee).copied() else {
            return Ty::Unknown;
        };
        let def_info = self.res.def(def).clone();
        match def_info.kind {
            DefKind::Builtin => self.type_builtin_call(id, &def_info.name, args, &arg_tys),
            DefKind::Fn | DefKind::Impl | DefKind::Spec => {
                let Some(item) = self.def_items.get(&def).copied() else {
                    return Ty::Unknown;
                };
                let Item::Fn { params, ret, .. } = self.ast.item(item).clone() else {
                    return Ty::Unknown;
                };
                if params.len() != args.len() {
                    self.error(
                        self.expr_span(id),
                        format!(
                            "`{}` takes {} argument(s), got {}",
                            def_info.name,
                            params.len(),
                            args.len()
                        ),
                    );
                    return Ty::Unknown;
                }
                // Match `bits[N]` params against known arg widths.
                let mut env: HashMap<DefId, u64> = HashMap::new();
                for (param, arg_ty) in params.iter().zip(&arg_tys) {
                    if let Some(pdef) = self.implicit_width_param(param.ty)
                        && let Ty::Bits(Width::Known(w)) = arg_ty
                        && let Some(prev) = env.insert(pdef, *w)
                        && prev != *w
                    {
                        self.error(
                            self.expr_span(id),
                            format!("conflicting widths for implicit parameter: {prev} vs {w}"),
                        );
                    }
                }
                // Check each arg against its (instantiated) param type.
                for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
                    let pty = self.eval_ty(param.ty, &env);
                    self.check_assignable(arg_ty, &pty, self.expr_span(*arg), "argument");
                }
                match ret {
                    Some(r) => self.eval_ty(r, &env),
                    None => Ty::Unit,
                }
            }
            DefKind::Rule => {
                self.error(
                    self.expr_span(id),
                    format!(
                        "rules are not callable (`{}` fires on its own)",
                        def_info.name
                    ),
                );
                Ty::Unknown
            }
            DefKind::Reg
            | DefKind::Mem
            | DefKind::Fifo
            | DefKind::Input
            | DefKind::Output
            | DefKind::Module => {
                self.error(
                    self.expr_span(id),
                    format!(
                        "`{}` is {}, not callable",
                        def_info.name,
                        def_info.kind.describe()
                    ),
                );
                Ty::Unknown
            }
            _ => Ty::Unknown,
        }
    }

    /// `bits[N]` where `N` is an implicit param -> that param's def.
    fn implicit_width_param(&self, ty: ExprId) -> Option<DefId> {
        let Expr::Bracket { callee, args } = self.ast.expr(ty) else {
            return None;
        };
        if !self.is_builtin(*callee, "bits") {
            return None;
        }
        let def = self.res.expr_defs.get(args.first()?)?;
        (self.res.def(*def).kind == DefKind::ImplicitParam).then_some(*def)
    }

    fn type_builtin_call(&mut self, id: ExprId, name: &str, args: &[ExprId], arg_tys: &[Ty]) -> Ty {
        match name {
            "clog2" | "len" => Ty::Int,
            "trunc" => {
                if args.len() != 2 {
                    self.error(
                        self.expr_span(id),
                        "`trunc` takes (value, width)".to_string(),
                    );
                    return Ty::Unknown;
                }
                match self.const_eval(args[1], &HashMap::new()) {
                    Some(w) => Ty::Bits(Width::Known(w)),
                    None => Ty::Bits(Width::Unknown),
                }
            }
            "pack" => {
                let mut total = 0u64;
                for t in arg_tys {
                    match t {
                        Ty::Bits(Width::Known(w)) => total += w,
                        _ => return Ty::Bits(Width::Unknown),
                    }
                }
                Ty::Bits(Width::Known(total))
            }
            "prio" => match arg_tys.first() {
                Some(Ty::Bits(Width::Known(w))) => Ty::Bits(Width::Known(clog2(*w).max(1))),
                _ => Ty::Bits(Width::Unknown),
            },
            // `logic(e)`: converts a fallible expression into a plain
            // boolean, always `bits[1]` regardless of `e`'s own type —
            // `e` is still type-checked normally above (as any argument
            // is), just its resulting Ty is discarded here. Whether `e`
            // is actually a fallible SHAPE (a fifo op, or a call to a
            // guard-only `<fails>` fn/impl) isn't a width/type question
            // — checked later in firrtl/checks.rs's `check_logic_args`,
            // once effects.rs's inferred signatures exist to consult,
            // matching where `check_failing_call_positions`/`check_
            // fifo_op_positions` already live for the same reason.
            "logic" => {
                if args.len() != 1 {
                    self.error(self.expr_span(id), "`logic` takes one argument".to_string());
                    return Ty::Unknown;
                }
                Ty::Bits(Width::Known(1))
            }
            "sync" => Ty::Unit,
            // Guard-only (`race[...]` as its own statement) never reads
            // this type; value-producing (`value := race[...]`) does —
            // the winner's own result type, once every named handle is
            // confirmed to share one. Args are handles (`Ty::Handle(T)`),
            // never the flattened internal form below (that's a DIFFERENT
            // builtin name, `__race_value`, never spelled `race`).
            "race" => {
                let mut result: Option<Ty> = None;
                for (arg, ty) in args.iter().zip(arg_tys) {
                    match ty {
                        Ty::Handle(inner) => match &result {
                            None => result = Some((**inner).clone()),
                            Some(prev) if prev != inner.as_ref() => {
                                self.error(
                                    self.expr_span(*arg),
                                    format!(
                                        "`race`'s handles must all share the same result \
                                         type; this one is {inner}, an earlier one was {prev}"
                                    ),
                                );
                            }
                            _ => {}
                        },
                        Ty::Unknown => {}
                        other => {
                            self.error(
                                self.expr_span(*arg),
                                format!("`race` needs a spawned handle, not {other}"),
                            );
                        }
                    }
                }
                result.unwrap_or(Ty::Unit)
            }
            // `__race_value[d1, r1, d2, r2, ...]` — lower.rs's own
            // rewrite of a value-producing `race[...]` at render time:
            // alternating done-flag/result-value pairs, already-renamed
            // real registers, never written by a user. `race`'s own
            // dispatch above already confirmed every result shares one
            // type before this was ever generated, so this just reads it
            // back off the first result arg — no arity/shape validation
            // a real source mistake could ever trigger here.
            "__race_value" => arg_tys.get(1).cloned().unwrap_or(Ty::Unknown),
            // any(range) is a model-checker free variable.
            "any" => Ty::Bits(Width::Unknown),
            _ => Ty::Unknown,
        }
    }
}
