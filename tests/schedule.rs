use trace::ast::{Ast, Item};
use trace::resolve::Resolution;
use trace::schedule::{ConflictKind, Exemption, Schedule, ScheduleError, schedule};
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
    assert_eq!(c.exemption, Exemption::None);
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
    reg a : [8] = 0
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
    reg a : [8] = 0
    reg x : [8] = 0
    reg y : [8] = 0
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
fn mutually_exclusive_exempts() {
    let src = "\
module M {
    reg a : [8] = 0
    rule p {
        a := a + 1
    }
    rule q {
        a := a + 2
    }
    schedule {
        mutually_exclusive { p, q }
    }
}
";
    let (_, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    assert_eq!(group.conflicts.len(), 1);
    assert_eq!(group.conflicts[0].exemption, Exemption::MutuallyExclusive);
}

#[test]
fn conflict_free_exempts_a_readwrite_conflict() {
    // `conflict_free` claims the OPPOSITE thing `mutually_exclusive`
    // does (safe to fire concurrently, not never-both-fire) -- both
    // waive the derived stall, but schedule.rs must keep them distinct
    // so firrtl/module.rs knows which one (if either) to check. Only
    // meaningful for a ReadWrite conflict (see
    // `conflict_free_rejects_a_writewrite_conflict` below) -- `p` writes
    // `a`, `q` reads it.
    let src = "\
module M {
    reg a : [8] = 0
    reg b : [8] = 0
    rule p {
        a := a + 1
    }
    rule q {
        b := a
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
    assert_eq!(group.conflicts[0].kind, ConflictKind::ReadWrite);
    assert_eq!(group.conflicts[0].exemption, Exemption::ConflictFree);
}

#[test]
fn conflict_free_rejects_a_writewrite_conflict() {
    // v0's emission model has one shared writer port/connect target per
    // resource, not two independent ones a WriteWrite pair could be
    // safely concurrent on -- `conflict_free` here would silently
    // compile to an unarbitrated last-connect race with no assertion
    // and no derived stall. Must be a real error, not accepted with a
    // footgun left in the emitted hardware.
    let src = "\
module M {
    reg a : [8] = 0
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
    let (_, _, _, errors) = run(src);
    assert!(!errors.is_empty());
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("mutually_exclusive"))
    );
}

#[test]
fn urgency_overrides_declaration_order() {
    let src = "\
module M {
    reg a : [8] = 0
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
    reg a : [8] = 0
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
    fifo f : [8]
    reg x : [8] = 0
    reg y : [8] = 0
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
fn fifo_contention_conflicts_through_a_let_binding_too() {
    // Same contention as `fifo_contention_conflicts`, but bound with
    // `let` instead of `:=` -- effects.rs's `infer_stmt` already walks
    // into a `Stmt::Let`'s init generically, so this was never actually
    // broken, unlike the firrtl.rs-level guard-gating gap the same
    // `let`-binding form exposed there.
    let src = "\
module M {
    fifo f : [8]
    reg x : [8] = 0
    reg y : [8] = 0
    rule p {
        let a = f.Deq[]
        x := a
    }
    rule q {
        let b = f.Deq[]
        y := b
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
fn different_instance_ports_do_not_conflict() {
    // Two rules writing DIFFERENT ports of the same instance must not
    // conflict: v0's conflict model is per-port, not per-instance (unlike
    // a mem array, a port name is static/lexical, so no runtime
    // disjointness proof is needed to tell them apart).
    let src = "\
module Child {
    in a : [8]
    in b : [8]
    out c : [8] = 0
    rule pass {
        c := a
    }
}
module Top {
    inst x : Child
    reg v : [8] = 0
    reg w : [8] = 0
    rule write_a {
        x.a := v
    }
    rule write_b {
        x.b := w
    }
}
";
    let (_, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = sched
        .groups
        .iter()
        .find(|g| g.order.len() == 2)
        .expect("Top's group");
    assert!(group.conflicts.is_empty());
}

#[test]
fn same_instance_port_conflicts() {
    // Two rules writing the SAME port of the same instance still
    // conflict, and the conflict names the port (`x.a`), not the whole
    // instance.
    let src = "\
module Child {
    in a : [8]
    out c : [8] = 0
    rule pass {
        c := a
    }
}
module Top {
    inst x : Child
    reg v : [8] = 0
    reg w : [8] = 0
    rule write_v {
        x.a := v
    }
    rule write_w {
        x.a := w
    }
}
";
    let (_, res, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = sched
        .groups
        .iter()
        .find(|g| g.order.len() == 2)
        .expect("Top's group");
    assert_eq!(group.conflicts.len(), 1);
    let on: Vec<&str> = group.conflicts[0]
        .on
        .iter()
        .map(|d| res.def(*d).name.as_str())
        .collect();
    assert_eq!(on, ["x.a"]);
}

#[test]
fn writing_a_port_and_reading_a_different_port_do_not_conflict() {
    let src = "\
module Child {
    in a : [8]
    out c : [8] = 0
    rule pass {
        c := a
    }
}
module Top {
    inst x : Child
    reg v : [8] = 0
    reg out : [8] = 0
    rule write_a {
        x.a := v
    }
    rule read_c {
        out := x.c
    }
}
";
    let (_, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = sched
        .groups
        .iter()
        .find(|g| g.order.len() == 2)
        .expect("Top's group");
    assert!(group.conflicts.is_empty());
}

#[test]
fn separate_modules_do_not_conflict() {
    let src = "\
module A {
    reg a : [8] = 0
    rule p {
        a := a + 1
    }
}

module B {
    reg a : [8] = 0
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
