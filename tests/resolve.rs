use trace::resolve::{DefKind, Resolution, ResolveError, resolve};
use trace::{
    ast::{Ast, Item},
    lexer, parser,
};

fn run(src: &str) -> (Ast, Resolution, Vec<ResolveError>) {
    let (tokens, lex_errors) = lexer::lex(src);
    assert!(lex_errors.is_empty(), "lex errors: {lex_errors:?}");
    let (ast, parse_errors) = parser::parse(src, &tokens);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let (res, errors) = resolve(&ast);
    (ast, res, errors)
}

fn run_ok(src: &str) -> (Ast, Resolution) {
    let (ast, res, errors) = run(src);
    assert!(errors.is_empty(), "resolve errors: {errors:?}");
    (ast, res)
}

/// Kinds of all defs that idents resolved to, for coarse assertions.
fn resolved_kinds(res: &Resolution) -> Vec<DefKind> {
    let mut kinds: Vec<_> = res.expr_defs.values().map(|d| res.def(*d).kind).collect();
    kinds.sort_by_key(|k| format!("{k:?}"));
    kinds.dedup();
    kinds
}

#[test]
fn locals_vs_state_writes() {
    // `let x = ...` binds a local; `count := ...` writes the register.
    let src = "\
module M {
    reg count : [8] = 0
    fifo input : [8]

    rule drain {
        let x = input.Deq[]
        count := count - 1
    }
}
";
    let (_, res) = run_ok(src);
    let kinds = resolved_kinds(&res);
    // `x` is declared but never READ, so it has no `expr_defs` entry to
    // pick up via `resolved_kinds` (only `Stmt::Assign`'s LHS gets one
    // at its own declaring site; `Stmt::Let`'s `name` has no `ExprId` at
    // all) -- check `res.defs` directly instead, same style already
    // used for `count`'s negative check below.
    assert!(
        res.defs
            .iter()
            .any(|d| d.name == "x" && d.kind == DefKind::Local),
        "x should be a local"
    );
    assert!(
        kinds.contains(&DefKind::Reg),
        "count should stay a register"
    );
    assert!(kinds.contains(&DefKind::Fifo));
    // No def named `count` of kind Local may exist.
    assert!(
        !res.defs
            .iter()
            .any(|d| d.name == "count" && d.kind == DefKind::Local)
    );
}

#[test]
fn unresolved_name_is_an_error() {
    let (_, _, errors) = run("rule t {\n let x = undeclared + 1\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("undeclared"));
}

#[test]
fn input_ports_are_read_only() {
    let (_, _, errors) = run("module M {\n in x : [8]\n \
         rule r {\n x := x + 1\n}\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("input port"));
    assert!(errors[0].message.contains("read-only"));
}

#[test]
fn output_ports_are_writable_state() {
    let (_, res) = run_ok(
        "module M {\n out x : [8] = 0\n \
         rule r {\n x := x + 1\n}\n}\n",
    );
    assert!(resolved_kinds(&res).contains(&DefKind::Output));
}

#[test]
fn io_ports_cannot_be_read_in_a_rule_body() {
    let (_, _, errors) = run("module M {\n io x : [8]\n reg y : [8] = 0\n \
         rule r {\n y := x\n}\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("io port"));
    assert!(errors[0].message.contains("attach"));
}

#[test]
fn io_ports_cannot_be_written_in_a_rule_body() {
    let (_, _, errors) = run("module M {\n io x : [8]\n \
         rule r {\n x := 8'd1\n}\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("io port"));
    assert!(errors[0].message.contains("attach"));
}

#[test]
fn io_ports_cannot_be_named_in_reads_or_writes() {
    let (_, _, errors) = run("module M {\n io x : [8]\n \
         rule r <reads {x}> {\n tick\n}\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("io port"));
}

#[test]
fn attach_resolves_two_bare_io_ports() {
    let (_, res) = run_ok("module M {\n io a : [8]\n io b : [8]\n attach a, b\n}\n");
    assert!(resolved_kinds(&res).contains(&DefKind::Io));
}

#[test]
fn attach_rejects_a_non_io_bare_operand() {
    let (_, _, errors) = run("module M {\n io a : [8]\n reg b : [8] = 0\n attach a, b\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot be attached"));
    assert!(errors[0].message.contains("not an io port"));
}

#[test]
fn attach_resolves_an_instance_io_port() {
    let (_, res) = run_ok(
        "module Child {\n io bus : [8]\n}\n\
         module Top {\n io bus : [8]\n inst c : Child\n attach bus, c.bus\n}\n",
    );
    // `c.bus` resolves through the generic InstPort scheme, same as any
    // other instance port -- kind checking (is `bus` actually `io` on
    // `Child`?) is types.rs's job, per the Input/Output precedent.
    assert!(resolved_kinds(&res).contains(&DefKind::InstPort));
}

#[test]
fn inst_accepts_an_extmodule_target() {
    let (_, res) = run_ok(
        "extmodule TriBuf from \"tribuf.v\" {\n in enable : [1]\n out sensed : [8]\n \
         io pad : [8]\n}\n\
         module Top {\n inst t : TriBuf\n}\n",
    );
    assert!(
        res.defs
            .iter()
            .any(|d| d.name == "t" && d.kind == DefKind::Inst)
    );
    assert!(
        res.defs
            .iter()
            .any(|d| d.name == "TriBuf" && d.kind == DefKind::ExtModule)
    );
}

#[test]
fn inst_rejects_a_target_that_is_neither_module_nor_extmodule() {
    let (_, _, errors) = run("struct Pair {\n x : [8]\n y : [8]\n}\n\
         module Top {\n inst t : Pair\n}\n");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("not a module or extmodule"))
    );
}

#[test]
fn inst_resolves_to_a_module_and_ports_are_fields() {
    let (_, res) = run_ok(
        "module Child {\n in a : [8]\n out b : [8] = 0\n \
         rule r {\n b := a\n}\n}\n\
         module Top {\n inst c : Child\n reg v : [8] = 0\n \
         rule w {\n c.a := v\n v := c.b\n}\n}\n",
    );
    assert!(resolved_kinds(&res).contains(&DefKind::Inst));
}

#[test]
fn nested_module_resolves_and_can_be_instantiated_from_its_parent() {
    let (_, res) = run_ok(
        "module Top {\n module Adder {\n in a : [8]\n out b : [8] = 0\n \
         rule r {\n b := a\n}\n}\n \
         inst c : Adder\n reg v : [8] = 0\n \
         rule w {\n c.a := v\n v := c.b\n}\n}\n",
    );
    assert!(resolved_kinds(&res).contains(&DefKind::Inst));
    assert!(
        res.defs
            .iter()
            .any(|d| d.name == "Adder" && d.kind == DefKind::Module)
    );
}

#[test]
fn nested_module_is_not_visible_outside_its_lexical_scope() {
    // `Adder` nested inside `Top` is scoped to `Top` (resolve.rs pushes a
    // fresh scope per module, popped on exit) — a sibling module can't
    // name it, same as a `reg`/`rule` declared inside one module was
    // already invisible to another.
    let (_, _, errors) = run(
        "module Top {\n module Adder {\n in a : [8]\n}\n inst c : Adder\n}\n\
         module Sibling {\n inst also_adder : Adder\n}\n",
    );
    assert!(errors.iter().any(|e| e.message.contains("cannot find")));
}

#[test]
fn nested_module_cannot_read_its_parents_state() {
    // A nested module's rule referencing its enclosing module's own `reg`
    // would resolve to a name that doesn't exist in the nested module's
    // own emitted FIRRTL scope (modules share nothing but ports) — must
    // be a clean resolve-time error, not a name that silently resolves
    // and only fails once it's invalid FIRRTL text firtool rejects.
    let (_, _, errors) = run("module Top {\n reg v : [8] = 0\n \
         module Adder {\n in a : [8]\n out sum : [8] = 0\n \
         rule add {\n sum := a + v\n}\n}\n inst c : Adder\n}\n");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("belongs to a different module"))
    );
}

#[test]
fn nested_module_cannot_write_its_parents_state() {
    let (_, _, errors) = run("module Top {\n reg v : [8] = 0\n \
         module Adder {\n in a : [8]\n rule set {\n v := a\n}\n}\n inst c : Adder\n}\n");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("belongs to a different module"))
    );
}

#[test]
fn nested_module_cannot_declare_reads_on_its_parents_state() {
    let (_, _, errors) = run("module Top {\n reg v : [8] = 0\n \
         module Adder {\n rule r <reads {v}> {\n tick\n}\n}\n inst c : Adder\n}\n");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("belongs to a different module"))
    );
}

#[test]
fn sibling_state_names_do_not_falsely_trigger_the_boundary_check() {
    // Two UNRELATED modules each declaring their own `v` must not
    // conflict with each other or with the boundary check (each `v`'s
    // owner is its own module; neither is an ancestor of the other, so
    // this must resolve via ordinary shadowing, not an error).
    run_ok(
        "module A {\n reg v : [8] = 0\n rule r {\n v := v + 1\n}\n}\n\
         module B {\n reg v : [8] = 0\n rule r {\n v := v + 1\n}\n}\n",
    );
}

#[test]
fn nested_module_shadowing_its_parents_state_name_binds_its_own() {
    // `Child`'s own `v` must win over `Top`'s `v` of the same name —
    // lookup is innermost-scope-first, so this should just work, but the
    // boundary check has to agree with that ordering rather than
    // second-guess it (this is the parent/child case the sibling test
    // above doesn't exercise: same name, one nested INSIDE the other).
    let (ast, res) = run_ok(
        "module Top {\n reg v : [8] = 0\n \
         module Child {\n reg v : [8] = 0\n rule r {\n v := v + 1\n}\n}\n inst c : Child\n}\n",
    );

    let Item::Module {
        items: top_items, ..
    } = ast.item(ast.roots[0])
    else {
        panic!("expected Top to be the sole root");
    };
    let top_v = top_items
        .iter()
        .find(|id| matches!(ast.item(**id), Item::Reg { name, .. } if name.text == "v"))
        .map(|id| res.item_defs[id])
        .expect("Top's own `v`");
    let child_item = *top_items
        .iter()
        .find(|id| matches!(ast.item(**id), Item::Module { name, .. } if name.text == "Child"))
        .expect("Child nested in Top");
    let Item::Module {
        items: child_items, ..
    } = ast.item(child_item)
    else {
        unreachable!()
    };
    let child_v = child_items
        .iter()
        .find(|id| matches!(ast.item(**id), Item::Reg { name, .. } if name.text == "v"))
        .map(|id| res.item_defs[id])
        .expect("Child's own `v`");
    assert_ne!(top_v, child_v, "sanity: two distinct `v` defs must exist");

    let ident_exprs: Vec<trace::ast::ExprId> = (0..ast.exprs.len())
        .map(|i| trace::ast::ExprId(i as u32))
        .filter(|id| matches!(ast.expr(*id), trace::ast::Expr::Ident(n) if n == "v"))
        .collect();
    assert_eq!(
        ident_exprs.len(),
        2,
        "expected two `v` idents in Child's rule"
    );
    for id in ident_exprs {
        assert_eq!(
            res.expr_defs[&id], child_v,
            "Child's rule must bind its OWN `v`, not Top's"
        );
    }
}

#[test]
fn inst_port_accesses_share_one_resource() {
    // `c.a`'s two occurrences must resolve to the SAME synthesized
    // `InstPort` def (effects.rs's conflict-set intersection depends on
    // this: two rules touching the same port need to land on one DefId,
    // not two distinct ones that happen to share a name).
    let (ast, res) = run_ok(
        "module Child {\n in a : [8]\n out b : [8] = 0\n \
         rule r {\n b := a\n}\n}\n\
         module Top {\n inst c : Child\n reg v : [8] = 0\n reg w : [8] = 0\n \
         rule p {\n c.a := v\n}\n rule q {\n c.a := w\n}\n}\n",
    );
    let field_exprs: Vec<trace::ast::ExprId> = (0..ast.exprs.len())
        .map(|i| trace::ast::ExprId(i as u32))
        .filter(|id| matches!(ast.expr(*id), trace::ast::Expr::Field { name, .. } if name == "a"))
        .collect();
    assert_eq!(field_exprs.len(), 2, "expected two `c.a` field exprs");
    let defs: Vec<_> = field_exprs.iter().map(|id| res.expr_defs[id]).collect();
    assert_eq!(defs[0], defs[1], "both `c.a` accesses must share one def");
    assert_eq!(res.def(defs[0]).kind, DefKind::InstPort);
    assert_eq!(res.def(defs[0]).name, "c.a");
}

#[test]
fn inst_target_must_be_a_module() {
    let (_, _, errors) = run("reg NotAModule : [1] = 0\nmodule M {\n inst x : NotAModule\n}\n");
    assert!(errors.iter().any(|e| e.message.contains("not a module")));
}

#[test]
fn struct_literal_naming_a_non_struct_is_an_error() {
    let (_, _, errors) = run("module NotAStruct {\n}\nmodule M {\n reg r : [8] = 0\n \
         rule x {\n r := NotAStruct{ a: 1 }\n }\n}\n");
    assert!(errors.iter().any(|e| e.message.contains("not a struct")));
}

#[test]
fn inst_cannot_be_assigned_directly() {
    let (_, _, errors) = run("module Child {\n in a : [8]\n}\n\
         module Top {\n inst c : Child\n rule w {\n c := c\n}\n}\n");
    assert!(errors.iter().any(|e| e.message.contains("module instance")));
}

#[test]
fn duplicate_definition_is_an_error() {
    let (_, _, errors) = run("module M {\n reg a : [1] = 0\n fifo a : [1]\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("already defined"));
    assert!(errors[0].message.contains("register"));
}

#[test]
fn let_shadowing_is_allowed() {
    run_ok("rule t {\n let x = 1\n let x = x + 1\n let y = x\n}\n");
}

#[test]
fn signature_types_bind_implicit_params() {
    let src = "\
spec AnyGrant(reqs : [N]) : [clog2(N)] <combines, chooses> {
    let i = any(0..N-1)
    reqs[i]?
    return i
}
";
    let (_, res) = run_ok(src);
    assert!(
        res.defs
            .iter()
            .any(|d| d.name == "N" && d.kind == DefKind::ImplicitParam)
    );
}

#[test]
fn body_names_do_not_bind_implicitly() {
    // Free names bind only in signature types, never in bodies.
    let (_, _, errors) = run("F(x : [8]) : [8] <combines> {\n return M\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot find `M`"));
}

#[test]
fn refines_must_name_a_spec() {
    let (_, _, errors) = run("impl I(x : [1]) : [1] <combines> refines Ghost {\n return x\n}\n");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("cannot find spec `Ghost`"))
    );

    let src = "\
Helper(x : [1]) : [1] <combines> {
    return x
}

impl I(x : [1]) : [1] <combines> refines Helper {
    return x
}
";
    let (_, _, errors) = run(src);
    assert!(errors.iter().any(|e| e.message.contains("not a spec")));
}

#[test]
fn effect_args_must_be_state() {
    let src = "\
module M {
    reg pc : [8] = 0
    rule r <reads {pc, ghost}> {
        tick
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot find state `ghost`"));

    let src = "\
module M {
    rule a {
        tick
    }
    rule r <reads {a}> {
        tick
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    // `a` is never in `scopes` at all now (it's in the separate `rule_
    // scopes` namespace — see resolve.rs's own doc comment on that
    // field), so `check_effect_args` reports it via its dedicated rule
    // fallback message, not the generic "not state (reg, mem, ...)" one.
    assert!(errors[0].message.contains("is a rule"));
}

#[test]
fn schedule_names_must_be_rules() {
    let src = "\
module M {
    reg pc : [8] = 0
    rule a {
        tick
    }
    schedule {
        urgency a > pc
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    // `pc` is a register, not a rule -- it was never in `rule_scopes` at
    // all (a truly separate namespace, not just a kind tag on one shared
    // map), so this is the same "cannot find" a schedule directive gets
    // for any nonexistent rule name, not a "wrong kind" message telling
    // you what `pc` actually is.
    assert!(errors[0].message.contains("cannot find rule `pc`"));
}

/// The whole point of the split: a rule and a piece of state may now
/// share a spelling with no collision — separate namespaces, separate
/// `DefId`s, exactly the shape `rule foo?` sugar's own `in foo` auto-
/// declaration needs (see TODO.md's "Rules: optional/enable sugar").
#[test]
fn a_rule_and_a_reg_may_share_a_name() {
    let (_, res) = run_ok("module M {\n reg foo : [1] = 0\n rule foo {\n foo := foo\n}\n}\n");
    assert!(
        res.defs
            .iter()
            .filter(|d| d.name == "foo")
            .map(|d| d.kind)
            .collect::<Vec<_>>()
            .contains(&DefKind::Rule)
    );
    assert!(
        res.defs
            .iter()
            .any(|d| d.name == "foo" && d.kind == DefKind::Reg)
    );
}

/// A rule's name can't be used as an ordinary expression value at all —
/// not just "can't be called" (the old, narrower check this replaces,
/// moved here from tests/types.rs: `t` no longer resolves as an `Ident`
/// anywhere outside a `schedule` directive, since `rule_scopes` is a
/// wholly separate namespace from `scopes`, so this is a plain "cannot
/// find" at resolve time, not a types-level "not callable" one call-site
/// shape used to catch on its own).
#[test]
fn a_rules_name_cannot_be_used_as_a_value() {
    let (_, _, errors) = run("module M {\n reg a : [8] = 0\n rule t {\n a := a + 1\n}\n \
         rule r {\n let x = t()\n}\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot find `t`"));
}

/// `declare_rule`'s own collision check (separate from `declare`'s, since
/// `rule_scopes` is a separate map) — two rules sharing a name must still
/// be rejected, just via the rule-specific error text.
#[test]
fn duplicate_rule_name_is_an_error() {
    let (_, _, errors) = run("module M {\n rule foo {\n tick\n}\n rule foo {\n tick\n}\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("already defined"));
    assert!(errors[0].message.contains("rule"));
}

/// `rule foo?`'s whole desugaring depends on `foo` naming BOTH the
/// synthesized `in` port and the rewritten rule, under separate `DefId`s
/// in separate namespaces (`scopes` vs `rule_scopes`) — exactly the
/// prerequisite `a_rule_and_a_reg_may_share_a_name` above exists to prove
/// in isolation. Resolves clean end to end, through the real parser's
/// desugaring, not a hand-built AST.
#[test]
fn optional_rule_sugar_resolves_with_no_namespace_collision() {
    let (_, res) = run_ok("module M {\n rule step? {\n tick\n}\n}\n");
    assert!(
        res.defs
            .iter()
            .any(|d| d.name == "step" && d.kind == DefKind::Input)
    );
    assert!(
        res.defs
            .iter()
            .any(|d| d.name == "step" && d.kind == DefKind::Rule)
    );
}

#[test]
fn rules_see_functions_declared_later() {
    let src = "\
module M {
    rule r {
        let x = Helper(1)
    }
}

Helper(v : [8]) : [8] <combines> {
    return v
}
";
    // Helper is top-level and declared after module M: still visible,
    // because scopes are two-phase.
    run_ok(src);
}

/// `let` is now the ONLY way to declare a fresh local (`x := e` on an
/// unbound name is a clean "cannot find" error instead, see the
/// `feature/require-let-for-locals` migration) -- inside a fn/impl body
/// specifically, a `let`-bound local that's never read back anywhere in
/// that same body is STILL its own separate rejection, dead by
/// construction since a fn's only outputs are its return value and its
/// state writes. This used to be reachable through a top-level fn
/// writing `log := d` meaning to reach a module's `log` reg it has no
/// lexical access to (`log` never resolved, so the write silently
/// became an unused local) -- that specific mistake is now caught
/// EARLIER and more directly, by the plain "cannot find `log`" error
/// (see `a_top_level_fn_writing_an_out_of_scope_name_is_a_clean_error`
/// below) -- but a genuinely fresh, unread `let` local inside a fn/impl
/// body (a typo'd return, forgotten field access, dead code) is a
/// distinct, still-live case this check alone catches.
#[test]
fn unread_local_in_a_fn_body_is_rejected() {
    let src = "\
Bump(d : [8]) : [8] <combines> {
    let doubled = d + d
    return d
}
module M {
    in x : [8]
    rule r {
        Bump(x)
    }
}
";
    let (_, _, errors) = run(src);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("assigned but never read")),
        "expected an unread-local rejection, got: {errors:?}"
    );
}

/// The scenario `unread_local_in_a_fn_body_is_rejected` used to pin
/// (before `let` became required for every fresh local): a top-level fn
/// writing `log := d` meaning to reach a module's `log` reg it has no
/// lexical access to. Now caught immediately by the ordinary "cannot
/// find" error -- `log` never resolves at all, so there's no unused
/// local left to even diagnose as unread.
#[test]
fn a_top_level_fn_writing_an_out_of_scope_name_is_a_clean_error() {
    let src = "\
Bump(d : [8]) : [8] <combines> {
    log := d
    return d
}
module M {
    in x : [8]
    reg log : [8] = 0
    rule r {
        Bump(x)
    }
}
";
    let (_, _, errors) = run(src);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("cannot find `log`")),
        "expected a cannot-find rejection, got: {errors:?}"
    );
}

/// The explicit-signature form of the same mistake (`<writes {log}>` on
/// a top-level fn that can't see any `log`) already had its own clean
/// error before this session -- `check_effect_args` rejects it directly,
/// unrelated to the new unread-local check. Pinned here alongside the
/// implicit-form test above so the two don't drift apart.
#[test]
fn explicit_writes_effect_naming_out_of_scope_state_is_rejected() {
    let src = "\
Bump(d : [8]) : [8] <combines, writes {log}> {
    log := d
    return d
}
module M {
    in x : [8]
    reg log : [8] = 0
    rule r {
        Bump(x)
    }
}
";
    let (_, _, errors) = run(src);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("cannot find state")),
        "expected a state-not-found rejection, got: {errors:?}"
    );
}

/// A fn/impl-body local that's actually read back (the overwhelmingly
/// common case -- every real example in the repo looks like this) type-
/// checks fine; a nested `if`/`else` local, declared and read at a
/// DEEPER scope than the fn's own top level, is also correctly seen (the
/// check walks `declared_locals` by declaration order across the WHOLE
/// body, not by scope-diffing the fn's own top-level scope object).
#[test]
fn read_locals_in_a_fn_body_are_unaffected_including_nested_in_if_else() {
    let src = "\
Classify(x : [8]) : [8] <combines> {
    let doubled = x + x
    if x > 10 {
        let big = doubled + 1
        return big
    } else {
        return doubled
    }
}
module M {
    in a : [8]
    out result : [8] = 0
    rule r {
        result := Classify(a)
    }
}
";
    run_ok(src);
}

/// The rule-level twin of the fn-body check above: a RULE's own bare
/// `x := e` scratch local is a legitimate, load-bearing pattern
/// (reassignment across statements, DESIGN.md's "Locals" section) and
/// must stay completely unaffected by the new fn/impl-only check --
/// `locals_vs_state_writes` above already has an unread rule-level
/// local (`x := input.Deq[]`, never read) passing `run_ok`, so this
/// just makes the "deliberately not checked here" scoping explicit as
/// its own named test.
#[test]
fn unread_local_in_a_rule_body_is_not_flagged() {
    let src = "\
module M {
    reg count : [8] = 0
    fifo input : [8]
    rule drain {
        let x = input.Deq[]
        count := count - 1
    }
}
";
    run_ok(src);
}

/// `if let x = opt? { ... } else { ... }`'s own `x` is branch-scoped to
/// `then_body` only -- a genuinely new scoping rule, unlike `Stmt::Let`'s
/// body-wide visibility. Reuses `Stmt::If`'s existing scope push/pop for
/// then/else (already isolates a plain `let` declared in one branch from
/// the other); `x` is just declared one level deeper, inside that same
/// pushed then-scope.
#[test]
fn if_let_bound_name_is_not_visible_in_the_else_branch() {
    let src = "\
module M {
    reg opt : ?[8] = false
    out result : [8] = 0
    rule r {
        if let x = opt? {
            result := x
        } else {
            result := x
        }
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot find `x`"));
}

#[test]
fn if_let_bound_name_is_not_visible_after_the_whole_statement() {
    let src = "\
module M {
    reg opt : ?[8] = false
    out result : [8] = 0
    rule r {
        if let x = opt? {
            result := x
        }
        result := x
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot find `x`"));
}

/// A plain `if`'s `then`/`else`-scoped `let` (no `if let` involved at
/// all) is already out of scope after the `if` closes, same as `if
/// let`'s own binding above — `Stmt::If`'s existing scope push/pop
/// already isolates it, unrelated to the branch-local EMISSION gap a
/// later session fixed (`reg_value_in_stmts` and its three siblings,
/// writes.rs, not resolve.rs) — confirmed here so that emission-level
/// fix, which binds a branch-local `let` into `self.locals` for its OWN
/// duration, is never mistaken for having widened resolve.rs's own
/// scoping.
#[test]
fn a_plain_ifs_branch_local_is_not_visible_after_the_branch_closes() {
    let src = "\
module M {
    reg flag : [1] = 0
    reg cnt : [8] = 0
    out result : [8] = 0
    rule r {
        if flag = 1 {
            let v = cnt - 1
            result := v
        }
        result := v
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot find `v`"));
}

#[test]
fn if_let_bound_name_shadows_silently_like_a_plain_let() {
    // `declare`'s existing shadow-allowance for `DefKind::Local` covers
    // `if let` too -- no special-casing needed, it's the same DefKind.
    let src = "\
module M {
    reg opt : ?[8] = false
    out result : [8] = 0
    rule r {
        let x = 8'd1
        if let x = opt? {
            result := x
        } else {
            result := x
        }
    }
}
";
    run_ok(src);
}

/// `while let v = opt? { ... }`'s own `v` is scoped to the loop body only
/// -- mirrors `if let`'s branch-scoping above, just with no `else` to
/// isolate it from (a loop has nothing to run once instead of looping).
#[test]
fn while_let_bound_name_is_not_visible_after_the_loop() {
    let src = "\
module M {
    reg opt : ?[8] = false
    out result : [8] = 0
    rule r <sequences> {
        while let v = opt? {
            result := v
        }
        result := v
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot find `v`"));
}

#[test]
fn where_bound_referencing_the_same_reg_resolves() {
    let src = "\
module M {
    reg i : [4] where i < 10 = 0
}
";
    run_ok(src);
}

#[test]
fn where_bound_referencing_wildcard_for_a_reg_resolves() {
    // Stage 3's own `_`-placeholder unification: a reg/out/param bound
    // used to require the literal variable name (unlike mem-element/
    // return/struct-field bounds, which have required `_` since their
    // own v18 retrofit) -- `_` is now accepted here too, alongside the
    // name, since it denotes the exact same thing (the def's own value)
    // and `bounds.rs`'s own collector never reads the LHS once this
    // resolves anyway.
    let src = "\
module M {
    reg i : [4] where _ < 10 = 0
}
";
    run_ok(src);
}

#[test]
fn where_bound_referencing_a_different_reg_is_an_error() {
    let src = "\
module M {
    reg i : [4] where j < 10 = 0
    reg j : [4] = 0
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("same reg/out/param it's declared on")
    );
}

#[test]
fn where_bound_referencing_the_same_param_resolves() {
    let src = "\
module M {
    reg x : [8] = 0
    Bump(i : [8] where i < 10) {
        x := i
    }
}
";
    run_ok(src);
}

#[test]
fn where_bound_referencing_wildcard_for_a_param_resolves() {
    let src = "\
module M {
    reg x : [8] = 0
    Bump(i : [8] where _ < 10) {
        x := i
    }
}
";
    run_ok(src);
}

#[test]
fn where_bound_referencing_a_different_param_is_an_error() {
    let src = "\
module M {
    reg x : [8] = 0
    Bump(i : [8] where x < 10) {
        x := i
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("same reg/out/param it's declared on")
    );
}

#[test]
fn wildcard_placeholder_resolves_in_a_return_bound() {
    // v13, retrofitted in v18: `_` is a shape placeholder for the
    // return value, not a real scoped binding -- resolved specially
    // (check_ret_bound_shape), not via ordinary identifier lookup.
    let src = "\
module M {
    Bump(i : [8]) : [8] where _ < 20 {
        return i
    }
}
";
    run_ok(src);
}

#[test]
fn return_bound_referencing_a_param_is_an_error() {
    let src = "\
module M {
    Bump(i : [8]) : [8] where i < 20 {
        return i
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("must reference the return value via `_`")
    );
}

#[test]
fn wildcard_placeholder_resolves_in_a_mem_bound() {
    // v17's own sibling of `wildcard_placeholder_resolves_in_a_return_
    // bound` above -- `_` is a shape placeholder for a mem's own
    // element, not a real scoped binding (checked by `check_mem_bound_
    // shape`, not ordinary identifier lookup).
    let src = "\
module M {
    mem m : [8][10] where _ < 50
}
";
    run_ok(src);
}

#[test]
fn mem_bound_referencing_the_mem_s_own_name_is_an_error() {
    let src = "\
module M {
    mem m : [8][10] where m < 50
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("must reference each element via `_`")
    );
}

#[test]
fn wildcard_placeholder_resolves_in_a_struct_field_bound() {
    // v18's own sibling of the mem/return-bound placeholder tests above
    // -- `_` is a shape placeholder for a struct field's own value, not
    // a real scoped binding (checked by `check_struct_field_bound_
    // shape`, not ordinary identifier lookup) -- a struct field has no
    // `DefId` of its own at all (`DefKind::Struct`'s own doc comment).
    let src = "\
struct Pair {
    valid : [1]
    data : [8] where _ < 50
}

module M {
    reg p : Pair = Pair{ valid: 0, data: 0 }
}
";
    run_ok(src);
}

#[test]
fn struct_field_bound_referencing_the_field_s_own_name_is_an_error() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8] where data < 50
}

module M {
    reg p : Pair = Pair{ valid: 0, data: 0 }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("must reference the field's own value via `_`")
    );
}

#[test]
fn where_bound_referencing_the_same_output_resolves() {
    let src = "\
module M {
    out i : [4] where i < 10 = 0
}
";
    run_ok(src);
}

#[test]
fn where_bound_referencing_wildcard_for_an_output_resolves() {
    let src = "\
module M {
    out i : [4] where _ < 10 = 0
}
";
    run_ok(src);
}

#[test]
fn where_bound_referencing_a_different_output_is_an_error() {
    let src = "\
module M {
    out i : [4] where j < 10 = 0
    out j : [4] = 0
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("same reg/out/param it's declared on")
    );
}

#[test]
fn all_examples_resolve() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples");
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "tr") {
            let src = std::fs::read_to_string(&path).unwrap();
            let (_, _, errors) = run(&src);
            assert!(errors.is_empty(), "resolve errors in {path:?}: {errors:?}");
        }
    }
}
