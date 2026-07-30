use trace::ast::{Ast, Item};
use trace::resolve::Resolution;
use trace::schedule::{ConflictKind, Schedule, ScheduleError, schedule};
use trace::{effects, lexer, parser, resolve};

fn run(src: &str) -> (Ast, Resolution, Schedule, Vec<ScheduleError>) {
    let (tokens, lex_errors) = lexer::lex(src);
    assert!(lex_errors.is_empty(), "lex errors: {lex_errors:?}");
    let (ast, parse_errors) = parser::parse(src, &tokens);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let (res, resolve_errors) = resolve::resolve(&ast);
    assert!(
        resolve_errors.is_empty(),
        "resolve errors: {resolve_errors:?}"
    );
    let (fx, effect_errors) = effects::check(&ast, &res);
    assert!(effect_errors.is_empty(), "effect errors: {effect_errors:?}");
    let (sched, errors) = schedule(&ast, &res, &fx);
    (ast, res, sched, errors)
}

fn rule_name(ast: &Ast, id: trace::ast::ItemId) -> String {
    match ast.item(id) {
        Item::Rule { name, .. } => name.text.clone(),
        _ => panic!("not a rule"),
    }
}

#[test]
fn subleq_schedule() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/subleq.tr"))
        .unwrap();
    let (ast, res, sched, errors) = run(&src);
    assert!(errors.is_empty(), "{errors:?}");
    let group = &sched.groups[0];
    assert!(group.directed, "urgency directive should shape the order");
    let order: Vec<String> = group.order.iter().map(|r| rule_name(&ast, *r)).collect();
    assert_eq!(order, ["step", "refill"]);

    // step writes m and pc; refill reads both: a conflict, step wins.
    assert_eq!(group.conflicts.len(), 1);
    let c = &group.conflicts[0];
    assert_eq!(rule_name(&ast, c.winner), "step");
    assert!(!c.exempted);
    let on: Vec<&str> = c.on.iter().map(|d| res.def(*d).name.as_str()).collect();
    assert!(
        on.contains(&"m"),
        "conflict must include the memory: {on:?}"
    );
}

#[test]
fn declaration_order_breaks_ties() {
    let src = "\
module M {
    reg a : bits[8] = 0
    rule first {
        a := a + 1
    }
    rule second {
        a := a + 2
    }
}
";
    let (ast, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    assert!(!group.directed);
    assert_eq!(group.conflicts.len(), 1);
    let c = &group.conflicts[0];
    assert_eq!(c.kind, ConflictKind::WriteWrite);
    assert_eq!(rule_name(&ast, c.winner), "first");
}

#[test]
fn read_read_does_not_conflict() {
    let src = "\
module M {
    reg a : bits[8] = 0
    reg x : bits[8] = 0
    reg y : bits[8] = 0
    rule p {
        x := a
    }
    rule q {
        y := a
    }
}
";
    let (_, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    assert!(sched.groups[0].conflicts.is_empty());
}

#[test]
fn conflict_free_exempts() {
    let src = "\
module M {
    reg a : bits[8] = 0
    rule p {
        a := a + 1
    }
    rule q {
        a := a + 2
    }
    schedule {
        conflict_free { p, q }
    }
}
";
    let (_, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    assert_eq!(group.conflicts.len(), 1);
    assert!(group.conflicts[0].exempted);
}

#[test]
fn urgency_overrides_declaration_order() {
    let src = "\
module M {
    reg a : bits[8] = 0
    rule p {
        a := a + 1
    }
    rule q {
        a := a + 2
    }
    schedule {
        urgency q > p
    }
}
";
    let (ast, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    let order: Vec<String> = group.order.iter().map(|r| rule_name(&ast, *r)).collect();
    assert_eq!(order, ["q", "p"]);
    assert_eq!(rule_name(&ast, group.conflicts[0].winner), "q");
}

#[test]
fn urgency_cycle_is_an_error() {
    let src = "\
module M {
    reg a : bits[8] = 0
    rule p {
        a := a + 1
    }
    rule q {
        a := a + 2
    }
    schedule {
        urgency p > q
        urgency q > p
    }
}
";
    let (_, _, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cycle"));
}

#[test]
fn fifo_contention_conflicts() {
    // Two rules dequeuing one fifo contend for it (fifo ops are
    // read+write conservatively).
    let src = "\
module M {
    fifo f : bits[8]
    reg x : bits[8] = 0
    reg y : bits[8] = 0
    rule p {
        x := f.Deq[]
    }
    rule q {
        y := f.Deq[]
    }
}
";
    let (_, res, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    assert_eq!(group.conflicts.len(), 1);
    let on: Vec<&str> = group.conflicts[0]
        .on
        .iter()
        .map(|d| res.def(*d).name.as_str())
        .collect();
    assert_eq!(on, ["f"]);
}

#[test]
fn explain_names_the_derived_stall() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/subleq.tr"))
        .unwrap();
    let (ast, res, sched, _) = run(&src);
    let text = sched.explain(&ast, &res);
    assert!(text.contains("module Subleq:"));
    assert!(text.contains("urgency: step > refill   (schedule directive)"));
    assert!(text.contains("rule step conflicts with rule refill"));
    assert!(text.contains("derived stall: refill fires only when step is blocked or idle"));
}

#[test]
fn separate_modules_do_not_conflict() {
    let src = "\
module A {
    reg a : bits[8] = 0
    rule p {
        a := a + 1
    }
}

module B {
    reg a : bits[8] = 0
    rule q {
        a := a + 1
    }
}
";
    let (_, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    for group in &sched.groups {
        assert!(group.conflicts.is_empty());
    }
}
