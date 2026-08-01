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
    // `x := ...` binds a local; `count := ...` writes the register.
    let src = "\
module M {
    reg count : bits[8] = 0
    fifo input : bits[8]

    rule drain {
        x := input.Deq[]
        count := count - 1
    }
}
";
    let (_, res) = run_ok(src);
    let kinds = resolved_kinds(&res);
    assert!(kinds.contains(&DefKind::Local), "x should be a local");
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
    let (_, _, errors) = run("rule t {\n x := undeclared + 1\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("undeclared"));
}

#[test]
fn input_ports_are_read_only() {
    let (_, _, errors) = run("module M {\n in x : bits[8]\n \
         rule r {\n x := x + 1\n}\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("input port"));
    assert!(errors[0].message.contains("read-only"));
}

#[test]
fn output_ports_are_writable_state() {
    let (_, res) = run_ok(
        "module M {\n out x : bits[8] = 0\n \
         rule r {\n x := x + 1\n}\n}\n",
    );
    assert!(resolved_kinds(&res).contains(&DefKind::Output));
}

#[test]
fn inst_resolves_to_a_module_and_ports_are_fields() {
    let (_, res) = run_ok(
        "module Child {\n in a : bits[8]\n out b : bits[8] = 0\n \
         rule r {\n b := a\n}\n}\n\
         module Top {\n inst c : Child\n reg v : bits[8] = 0\n \
         rule w {\n c.a := v\n v := c.b\n}\n}\n",
    );
    assert!(resolved_kinds(&res).contains(&DefKind::Inst));
}

#[test]
fn nested_module_resolves_and_can_be_instantiated_from_its_parent() {
    let (_, res) = run_ok(
        "module Top {\n module Adder {\n in a : bits[8]\n out b : bits[8] = 0\n \
         rule r {\n b := a\n}\n}\n \
         inst c : Adder\n reg v : bits[8] = 0\n \
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
        "module Top {\n module Adder {\n in a : bits[8]\n}\n inst c : Adder\n}\n\
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
    let (_, _, errors) = run("module Top {\n reg v : bits[8] = 0\n \
         module Adder {\n in a : bits[8]\n out sum : bits[8] = 0\n \
         rule add {\n sum := a + v\n}\n}\n inst c : Adder\n}\n");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("belongs to a different module"))
    );
}

#[test]
fn nested_module_cannot_write_its_parents_state() {
    let (_, _, errors) = run("module Top {\n reg v : bits[8] = 0\n \
         module Adder {\n in a : bits[8]\n rule set {\n v := a\n}\n}\n inst c : Adder\n}\n");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("belongs to a different module"))
    );
}

#[test]
fn nested_module_cannot_declare_reads_on_its_parents_state() {
    let (_, _, errors) = run("module Top {\n reg v : bits[8] = 0\n \
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
        "module A {\n reg v : bits[8] = 0\n rule r {\n v := v + 1\n}\n}\n\
         module B {\n reg v : bits[8] = 0\n rule r {\n v := v + 1\n}\n}\n",
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
        "module Top {\n reg v : bits[8] = 0\n \
         module Child {\n reg v : bits[8] = 0\n rule r {\n v := v + 1\n}\n}\n inst c : Child\n}\n",
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
        "module Child {\n in a : bits[8]\n out b : bits[8] = 0\n \
         rule r {\n b := a\n}\n}\n\
         module Top {\n inst c : Child\n reg v : bits[8] = 0\n reg w : bits[8] = 0\n \
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
    let (_, _, errors) = run("reg NotAModule : bits[1] = 0\nmodule M {\n inst x : NotAModule\n}\n");
    assert!(errors.iter().any(|e| e.message.contains("not a module")));
}

#[test]
fn inst_cannot_be_assigned_directly() {
    let (_, _, errors) = run("module Child {\n in a : bits[8]\n}\n\
         module Top {\n inst c : Child\n rule w {\n c := c\n}\n}\n");
    assert!(errors.iter().any(|e| e.message.contains("module instance")));
}

#[test]
fn duplicate_definition_is_an_error() {
    let (_, _, errors) = run("module M {\n reg a : bits[1] = 0\n fifo a : bits[1]\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("already defined"));
    assert!(errors[0].message.contains("register"));
}

#[test]
fn let_shadowing_is_allowed() {
    run_ok("rule t {\n let x = 1\n let x = x + 1\n y := x\n}\n");
}

#[test]
fn signature_types_bind_implicit_params() {
    let src = "\
spec AnyGrant(reqs : bits[N]) : bits[clog2(N)] <combines, chooses> {
    i := any(0..N-1)
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
    let (_, _, errors) = run("F(x : bits[8]) : bits[8] <combines> {\n return M\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot find `M`"));
}

#[test]
fn refines_must_name_a_spec() {
    let (_, _, errors) =
        run("impl I(x : bits[1]) : bits[1] <combines> refines Ghost {\n return x\n}\n");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("cannot find spec `Ghost`"))
    );

    let src = "\
Helper(x : bits[1]) : bits[1] <combines> {
    return x
}

impl I(x : bits[1]) : bits[1] <combines> refines Helper {
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
    reg pc : bits[8] = 0
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
    assert!(errors[0].message.contains("not state"));
}

#[test]
fn schedule_names_must_be_rules() {
    let src = "\
module M {
    reg pc : bits[8] = 0
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
    assert!(errors[0].message.contains("not a rule"));
}

#[test]
fn rules_see_functions_declared_later() {
    let src = "\
module M {
    rule r {
        x := Helper(1)
    }
}

Helper(v : bits[8]) : bits[8] <combines> {
    return v
}
";
    // Helper is top-level and declared after module M: still visible,
    // because scopes are two-phase.
    run_ok(src);
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
