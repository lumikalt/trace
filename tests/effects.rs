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
fn tick_requires_suspends() {
    let (_, _, errors) = run("rule t {\n tick\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("`tick` requires `<suspends>`"));

    run_ok("rule t <suspends> {\n tick\n}\n");
}

#[test]
fn while_needs_suspends_or_allocates() {
    // DESIGN.md's E012 example.
    let (_, _, errors) = run(
        "Bad(x : bits[8]) : bits[8] <converges> {\n while x != 0 { x := x >> 1 }\n return x\n}\n",
    );
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("one iteration per cycle"));

    run_ok("Ok(x : bits[8]) : bits[8] <suspends> {\n while x != 0 { x := x >> 1 }\n return x\n}\n");
}

#[test]
fn any_requires_choice() {
    let (_, _, errors) = run("F(x : bits[4]) : bits[2] <converges> {\n return any(0..3)\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("`any`"));
    assert!(errors[0].message.contains("<choice>"));
}

#[test]
fn choice_only_on_specs() {
    let (_, _, errors) =
        run("F(x : bits[4]) : bits[2] <converges, choice> {\n return any(0..3)\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("only a `spec`"));

    run_ok("spec S(x : bits[4]) : bits[2] <converges, choice> {\n return any(0..3)\n}\n");
}

#[test]
fn contradictory_colors() {
    let (_, _, errors) = run("F(x : bits[1]) : bits[1] <converges, suspends> {\n return x\n}\n");
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
fn calling_suspends_needs_suspends() {
    let src = "\
Slow(x : bits[8]) : bits[8] <suspends> {
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
            .contains("calling `<suspends>` function `Slow`")
    );
}

#[test]
fn specs_are_not_callable() {
    let src = "\
spec S(x : bits[1]) : bits[1] <converges, choice> {
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
fn decides_infers_through_calls() {
    let src = "\
Classify(x : bits[8]) : bits[8] <converges> {
    (x != 0)?
    return x
}

Wrap(x : bits[8]) : bits[8] <converges> {
    return Classify(x)
}
";
    let (ast, fx) = run_ok(src);
    assert!(fx.sigs[&item_named(&ast, "Classify")].decides);
    assert!(
        fx.sigs[&item_named(&ast, "Wrap")].decides,
        "decides must propagate through the call"
    );
}

#[test]
fn fifo_ops_infer_decides_and_rows() {
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
    assert!(sig.decides, "fifo op makes the rule fallible");
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
fn recursion_requires_allocates() {
    let (_, _, errors) = run("F(x : bits[8]) : bits[8] <converges> {\n return F(x)\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("recursive"));
    assert!(errors[0].message.contains("<allocates>"));

    // AdderTree-style elaboration recursion is legal.
    run_ok(
        "G(x : bits[8]) : bits[8] <allocates> {\n if x == 0 { return 0 }\n return G(x - 1)\n}\n",
    );
}

#[test]
fn guards_forbidden_at_elaboration_time() {
    // In an initializer.
    let (_, _, errors) = run("module M {\n reg a : bits[8] = 0\n reg b : bits[8] = a?\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("elaboration time"));

    // In an <allocates> body.
    let (_, _, errors) = run("H(x : bits[8]) : bits[8] <allocates> {\n (x != 0)?\n return x\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("elaboration time"));
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
