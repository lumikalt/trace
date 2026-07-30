use trace::resolve::{DefKind, Resolution, ResolveError, resolve};
use trace::{ast::Ast, lexer, parser};

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
    let (_, _, errors) = run("module M {\n input x : bits[8]\n \
         rule r {\n x := x + 1\n}\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("input port"));
    assert!(errors[0].message.contains("read-only"));
}

#[test]
fn output_ports_are_writable_state() {
    let (_, res) = run_ok(
        "module M {\n output x : bits[8] = 0\n \
         rule r {\n x := x + 1\n}\n}\n",
    );
    assert!(resolved_kinds(&res).contains(&DefKind::Output));
}

#[test]
fn inst_resolves_to_a_module_and_ports_are_fields() {
    let (_, res) = run_ok(
        "module Child {\n input a : bits[8]\n output b : bits[8] = 0\n \
         rule r {\n b := a\n}\n}\n\
         module Top {\n inst c : Child\n reg v : bits[8] = 0\n \
         rule w {\n c.a := v\n v := c.b\n}\n}\n",
    );
    assert!(resolved_kinds(&res).contains(&DefKind::Inst));
}

#[test]
fn inst_target_must_be_a_module() {
    let (_, _, errors) = run("reg NotAModule : bits[1] = 0\nmodule M {\n inst x : NotAModule\n}\n");
    assert!(errors.iter().any(|e| e.message.contains("not a module")));
}

#[test]
fn inst_cannot_be_assigned_directly() {
    let (_, _, errors) = run("module Child {\n input a : bits[8]\n}\n\
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
