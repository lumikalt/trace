//! The setup/collection pass that runs before per-body checking: every
//! reg/mem/fifo/input/output/io's declared type (`collect_state`), every
//! struct's declared field list (`collect_structs`, plus the cycle check
//! that keeps struct flattening terminating), every module's port list
//! and instance target (`collect_module_ports`), `attach` operand
//! validation (`check_attaches`), and the per-body driver that walks
//! every rule/fn to a fixed point (`check_all`/`check_body`, dispatching
//! into stmt.rs). `check_destructures` runs last of all (see `check`'s
//! own doc comment, mod.rs) since it needs every body's `expr_tys`
//! already populated.

use super::{OPTION_FIELDS, Ty, TypeChecker, WIDEN_CAP};
use crate::ast::{Expr, ExprId, ExtPortDir, Item, ItemId};
use crate::resolve::{DefId, DefKind};
use std::collections::HashMap;

impl<'a> TypeChecker<'a> {
    // --- declared state types ---

    pub(crate) fn collect_state(&mut self) {
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
                // `io` lowers to FIRRTL's `Analog<N>`, a plain N-bit net —
                // no struct/Option shape (v0 restriction: neither has an
                // established Analog-bundle equivalent, and nothing
                // downstream needs one since an io port is never read or
                // written as a value, only `attach`ed whole).
                Item::Io { ty, .. } => {
                    let ty = self.eval_ty(ty, &HashMap::new());
                    if !matches!(ty, Ty::Bits(_) | Ty::Unknown) {
                        let span = self.ast.item_spans[id.0 as usize].clone();
                        self.error(
                            span,
                            format!("an io port holds a plain bit width, not {ty}"),
                        );
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
    pub(crate) fn collect_structs(&mut self) {
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
    pub(crate) fn check_destructures(&mut self) {
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
    pub(crate) fn collect_module_ports(&mut self) {
        // Was purely mechanical (no `self.error` calls) before extmodule
        // ports needed their own shape validation here — needs its own
        // `emit = true`/`= false` bracket now, same reason `collect_
        // structs`'s own doc comment gives.
        self.emit = true;
        let mut stack: Vec<ItemId> = self.ast.roots.clone();
        while let Some(id) = stack.pop() {
            match self.ast.item(id).clone() {
                Item::Module { items, .. } => {
                    if let Some(&module_def) = self.res.item_defs.get(&id) {
                        let mut ports = Vec::new();
                        for item_id in &items {
                            match self.ast.item(*item_id) {
                                Item::Input { .. } | Item::Output { .. } | Item::Io { .. } => {
                                    if let Some(&def) = self.res.item_defs.get(item_id) {
                                        let kind = self.res.def(def).kind;
                                        let ty = self
                                            .state_tys
                                            .get(&def)
                                            .cloned()
                                            .unwrap_or(Ty::Unknown);
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
                    stack.extend(items);
                }
                // An extmodule's own port list is never `Item::Input`/
                // `Output`/`Io` sub-items (unlike a real module's own
                // body — see `ast::ExtPort`'s doc comment for why: they
                // never need their own `DefId`), so it needs its own
                // small port-list builder here rather than reusing the
                // walk above.
                Item::ExtModule { ports, .. } => {
                    if let Some(&extmodule_def) = self.res.item_defs.get(&id) {
                        let built = ports
                            .iter()
                            .map(|p| {
                                let kind = match p.dir {
                                    ExtPortDir::In => DefKind::Input,
                                    ExtPortDir::Out => DefKind::Output,
                                    ExtPortDir::Io => DefKind::Io,
                                };
                                let ty = self.eval_ty(p.ty, &HashMap::new());
                                if kind == DefKind::Io && !matches!(ty, Ty::Bits(_) | Ty::Unknown) {
                                    self.error(
                                        self.expr_span(p.ty),
                                        format!("an io port holds a plain bit width, not {ty}"),
                                    );
                                } else if kind != DefKind::Io
                                    && !matches!(
                                        ty,
                                        Ty::Bits(_)
                                            | Ty::Struct { .. }
                                            | Ty::Option(_)
                                            | Ty::Unknown
                                    )
                                {
                                    self.error(
                                        self.expr_span(p.ty),
                                        format!(
                                            "an extmodule port holds bits or a struct, not {ty}"
                                        ),
                                    );
                                }
                                (p.name.text.clone(), kind, ty)
                            })
                            .collect();
                        self.types.module_ports.insert(extmodule_def, built);
                    }
                }
                _ => {}
            }
        }
        self.emit = false;
    }

    /// `base` names a module instance -> the module `DefId` it instantiates.
    pub(crate) fn instance_module_of(&self, base: ExprId) -> Option<DefId> {
        let def = *self.res.expr_defs.get(&base)?;
        self.types.instance_module.get(&def).copied()
    }

    pub(crate) fn find_port(&self, module_def: DefId, name: &str) -> Option<(DefKind, Ty)> {
        self.types
            .module_ports
            .get(&module_def)?
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, k, t)| (*k, t.clone()))
    }

    /// `attach a, b`'s type/kind check for one operand — resolve.rs has
    /// already restricted a bare-name operand to `DefKind::Io` (see
    /// `resolve_attach_operand`), so only the `inst.port` shape (and
    /// anything not even Ident/Field, which resolve.rs lets through
    /// unrestricted since it isn't a bare name) needs checking here.
    fn attach_operand(&mut self, id: ExprId) -> Option<Ty> {
        match self.ast.expr(id).clone() {
            Expr::Ident(_) => {
                let def = self.res.expr_defs.get(&id).copied()?;
                self.state_tys.get(&def).cloned()
            }
            Expr::Field { base, name } => {
                // Unlike an ordinary `.field` read/write (which falls back
                // to struct/handle field access when `base` isn't an
                // instance), an attach operand's ONLY legal `.field` shape
                // is `instance.port` — falling through silently here would
                // let a sibling module's own NAME (not a bound `inst`)
                // reach emission unchecked, producing FIRRTL referencing a
                // declaration that doesn't exist in this module's scope
                // (self-caught: `attach bus, A.bus` where `A` is a module,
                // not an `inst c : A`, emitted clean with zero trace error
                // and firtool then rejected it with `use of unknown
                // declaration 'A'` — a confusing raw-FIRRTL error instead
                // of a real one).
                let Some(module_def) = self.instance_module_of(base) else {
                    self.error(
                        self.expr_span(id),
                        "an attach operand must be an io port name, or `instance.port` \
                         (this isn't a reference to a module instance)"
                            .to_string(),
                    );
                    return None;
                };
                self.types.expr_tys.insert(base, Ty::Unknown);
                match self.find_port(module_def, &name) {
                    Some((DefKind::Io, ty)) => Some(ty),
                    Some((_, _)) => {
                        self.error(
                            self.expr_span(id),
                            format!(
                                "cannot attach `.{name}`: it is not an io port on this instance"
                            ),
                        );
                        None
                    }
                    None => {
                        self.error(
                            self.expr_span(id),
                            format!("this instance has no port `{name}`"),
                        );
                        None
                    }
                }
            }
            _ => {
                self.error(
                    self.expr_span(id),
                    "an attach operand must be an io port name, or `instance.port`".to_string(),
                );
                None
            }
        }
    }

    /// `attach a, b`: both operands must be `io` ports (checked per-
    /// operand by `attach_operand`) of the same width — FIRRTL's own
    /// `attach` doesn't itself require matching widths between an
    /// arbitrary N operands, but two DIFFERENTLY-sized `Analog` nets
    /// wired together has no sensible meaning for this language's model,
    /// so it's rejected here rather than left to whatever firtool would
    /// do with it.
    pub(crate) fn check_attaches(&mut self) {
        self.emit = true;
        let mut stack: Vec<ItemId> = self.ast.roots.clone();
        while let Some(id) = stack.pop() {
            match self.ast.item(id).clone() {
                Item::Module { items, .. } => stack.extend(items),
                Item::Attach { a, b } => {
                    let ta = self.attach_operand(a);
                    let tb = self.attach_operand(b);
                    if let (Some(ta), Some(tb)) = (ta, tb)
                        && ta != tb
                    {
                        self.error(
                            self.ast.item_spans[id.0 as usize].clone(),
                            format!("attach operands must have the same type: {ta} vs {tb}"),
                        );
                    }
                }
                _ => {}
            }
        }
        self.emit = false;
    }

    // --- bodies ---

    pub(crate) fn check_all(&mut self) {
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
}
