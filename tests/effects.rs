use trace::ast::{Ast, Item};
use trace::effects::{EffectError, Effects, check};
use trace::{lexer, parser, resolve};

fn run(src: &str) -> (Ast, Effects, Vec<EffectError>) {
    let (tokens, lex_errors) = lexer::lex(src);
    assert!(lex_errors.is_empty(), "lex errors: {lex_errors:?}");
    let (ast, parse_errors) = parser::parse(src, &tokens);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let (res, resolve_errors) = resolve::resolve(&ast);
    assert!(
        resolve_errors.is_empty(),
        "resolve errors: {resolve_errors:?}"
    );
    let (fx, errors) = check(&ast, &res);
    (ast, fx, errors)
}

fn run_ok(src: &str) -> (Ast, Effects) {
    let (ast, fx, errors) = run(src);
    assert!(errors.is_empty(), "effect errors: {errors:?}");
    (ast, fx)
}

/// Find the item id of the fn or rule with the given name.
fn item_named(ast: &Ast, wanted: &str) -> trace::ast::ItemId {
    for (i, item) in ast.items.iter().enumerate() {
        let name = match item {
            Item::Rule { name, .. } | Item::Fn { name, .. } => &name.text,
            _ => continue,
        };
        if name == wanted {
            return trace::ast::ItemId(i as u32);
        }
    }
    panic!("no item named {wanted}");
}

#[test]
fn tick_requires_sequences() {
    let (_, _, errors) = run("rule t {\n tick\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("`tick` requires `<sequences>`"));

    run_ok("rule t <sequences> {\n tick\n}\n");
}

#[test]
fn return_is_rejected_inside_a_rule() {
    // A `rule` has no return value -- found while auditing `_ => {}`
    // wildcard matches for the Assign-vs-Let bug class: nothing
    // downstream had a case for `Stmt::Return` at a rule's top level, so
    // `return f.Deq[]` used to compile clean and silently drop the whole
    // statement (the fifo op, and anything else it wrapped) with zero
    // emitted logic and zero error.
    let (_, _, errors) = run("fifo f : bits[8]\nrule r {\n return f.Deq[]\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("`return` is only valid inside a function body")
    );

    // Still legal inside a real fn/spawn-callee body.
    run_ok("F(x : bits[8]) : bits[8] <combines> {\n return x\n}\n");
}

#[test]
fn while_needs_sequences_or_elaborates() {
    // DESIGN.md's E012 example.
    let (_, _, errors) = run(
        "Bad(x : bits[8]) : bits[8] <combines> {\n while x != 0 { x := x >> 1 }\n return x\n}\n",
    );
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("one iteration per cycle"));

    run_ok(
        "Ok(x : bits[8]) : bits[8] <sequences> {\n while x != 0 { x := x >> 1 }\n return x\n}\n",
    );
}

#[test]
fn any_requires_chooses() {
    let (_, _, errors) = run("F(x : bits[4]) : bits[2] <combines> {\n return any(0..3)\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("`any`"));
    assert!(errors[0].message.contains("<chooses>"));
}

#[test]
fn chooses_only_on_specs() {
    let (_, _, errors) =
        run("F(x : bits[4]) : bits[2] <combines, chooses> {\n return any(0..3)\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("only a `spec`"));

    run_ok("spec S(x : bits[4]) : bits[2] <combines, chooses> {\n return any(0..3)\n}\n");
}

#[test]
fn contradictory_colors() {
    let (_, _, errors) = run("F(x : bits[1]) : bits[1] <combines, sequences> {\n return x\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("contradicts"));
}

#[test]
fn unknown_effect_name() {
    let (_, _, errors) = run("F(x : bits[1]) : bits[1] <transacts> {\n return x\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("unknown effect `transacts`"));
}

#[test]
fn calling_sequences_needs_sequences() {
    let src = "\
Slow(x : bits[8]) : bits[8] <sequences> {
    tick
    return x
}

rule r {
    y := Slow(1)
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("calling `<sequences>` function `Slow`")
    );
}

#[test]
fn specs_are_not_callable() {
    let src = "\
spec S(x : bits[1]) : bits[1] <combines, chooses> {
    return any(0..1)
}

rule r {
    y := S(1)
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("verification-only"));
}

#[test]
fn fails_infers_through_calls() {
    let src = "\
Classify(x : bits[8]) : bits[8] <combines> {
    (x != 0)?
    return x
}

Wrap(x : bits[8]) : bits[8] <combines> {
    return Classify(x)
}
";
    let (ast, fx) = run_ok(src);
    assert!(fx.sigs[&item_named(&ast, "Classify")].fails);
    assert!(
        fx.sigs[&item_named(&ast, "Wrap")].fails,
        "fails must propagate through the call"
    );
}

#[test]
fn fifo_ops_infer_fails_and_rows() {
    let src = "\
module M {
    fifo input : bits[8]
    reg count : bits[8] = 0

    rule drain {
        x := input.Deq[]
        count := count - 1
    }
}
";
    let (ast, fx) = run_ok(src);
    let sig = &fx.sigs[&item_named(&ast, "drain")];
    assert!(sig.fails, "fifo op makes the rule fallible");
    assert_eq!(sig.reads.len(), 2, "input + count");
    assert_eq!(sig.writes.len(), 2, "input + count");
}

#[test]
fn row_understatement_is_an_error() {
    let src = "\
module M {
    reg pc : bits[8] = 0
    mem m : bits[8][256]

    rule r <reads {pc}> {
        x := m[pc]
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("reads `m`"));

    // Overstating is allowed: conservative rows are sound.
    let src = "\
module M {
    reg pc : bits[8] = 0
    mem m : bits[8][256]

    rule r <reads {pc, m}, writes {pc, m}> {
        pc := pc + 1
    }
}
";
    let (_, _, errors) = run(src);
    assert!(
        errors.is_empty(),
        "overstating rows must be legal: {errors:?}"
    );
}

#[test]
fn recursion_requires_elaborates() {
    let (_, _, errors) = run("F(x : bits[8]) : bits[8] <combines> {\n return F(x)\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("recursive"));
    assert!(errors[0].message.contains("<elaborates>"));

    // AdderTree-style elaboration recursion is legal.
    run_ok(
        "G(x : bits[8]) : bits[8] <elaborates> {\n if x == 0 { return 0 }\n return G(x - 1)\n}\n",
    );
}

#[test]
fn guards_forbidden_at_elaboration_time() {
    // In an initializer.
    let (_, _, errors) = run("module M {\n reg a : bits[8] = 0\n reg b : bits[8] = a?\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("elaboration time"));

    // In an <elaborates> body.
    let (_, _, errors) = run("H(x : bits[8]) : bits[8] <elaborates> {\n (x != 0)?\n return x\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("elaboration time"));
}

#[test]
fn spawn_callee_must_itself_be_sequences() {
    let src = "\
Fast(x : bits[8]) : bits[8] <combines> {
    return x
}

rule r <sequences> {
    h := spawn Fast(1)
    tick
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("must itself be declared"));

    // A <sequences> callee is fine.
    run_ok(
        "Slow(x : bits[8]) : bits[8] <sequences> {\n tick\n return x\n}\n\n\
         rule r <sequences> {\n h := spawn Slow(1)\n tick\n}\n",
    );
}

#[test]
fn spawn_needs_a_direct_call() {
    let src = "\
rule r <sequences> {
    h := spawn 1
    tick
}
";
    let (_, _, errors) = run(src);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("direct function call"))
    );
}

#[test]
fn bracket_builtin_call_gets_the_same_checks_as_a_paren_call() {
    // `sync`/`race` are called with brackets (`sync[...]`), a distinct AST
    // shape (`Expr::Bracket`) from an ordinary paren call (`Expr::Call`) —
    // the `<sequences>`-required check has to run for that shape too, not
    // just the paren-call one, or `sync[...]` in an ordinary `combines`
    // rule would silently pass.
    let (_, _, errors) = run("module M {\n reg h : bits[1] = 0\n rule t {\n w := sync[h]\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("`sync` requires `<sequences>`"));

    let (_, _, errors) = run("module M {\n reg h : bits[1] = 0\n rule t {\n w := race[h]\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("`race` requires `<sequences>`"));
}

#[test]
fn all_examples_pass_effect_check() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples");
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "tr") {
            let src = std::fs::read_to_string(&path).unwrap();
            let (_, _, errors) = run(&src);
            assert!(errors.is_empty(), "effect errors in {path:?}: {errors:?}");
        }
    }
}
