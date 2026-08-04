use trace::ast::{Ast, Item};
use trace::resolve::Resolution;
use trace::schedule::{ConflictKind, Exemption, Schedule, ScheduleError, schedule};
use trace::{effects, lexer, parser, resolve, types};

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
    let (ty, type_errors) = types::check(&ast, &res, &fx);
    assert!(type_errors.is_empty(), "type errors: {type_errors:?}");
    let (sched, errors) = schedule(&ast, &res, &fx, &ty);
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
fn conflict_free_mem_example_still_needs_its_own_annotation() {
    // examples/conflict_free_mem.tr addresses `m` through runtime input
    // ports (`write_addr`/`read_addr`), not compile-time constants -- the
    // new auto-proof must fail closed here exactly as before this
    // feature existed, leaving the user's own `conflict_free` claim as
    // the only reason the derived stall is waived.
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/conflict_free_mem.tr"
    ))
    .unwrap();
    let (_, _, sched, errors) = run(&src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    assert_eq!(group.conflicts.len(), 1);
    assert_eq!(group.conflicts[0].exemption, Exemption::ConflictFree);
}

#[test]
fn mem_disjoint_proof_reaches_a_read_via_a_callee() {
    // The index table merges through the call-graph fixpoint
    // (effects.rs's `mem_read_idx`/`mem_write_idx`, see
    // tests/effects.rs's `mem_index_sites_merge_through_a_callee_call_
    // graph`) -- this pins that the resulting exemption is what
    // schedule.rs actually derives for the pair, not just that the
    // table itself gets populated. FIRRTL emission for a mem read
    // reached through a callee is a separate, pre-existing, unrelated
    // restriction (v0 restriction: "this indexing form is not yet
    // supported") -- the exemption is correct here regardless of
    // whether emission can act on it yet.
    let src = "\
module M {
    mem m : [8][16]
    in x : [8]
    out y : [8] = 0

    Fetch() : [8] {
        return m[7]
    }

    rule p {
        m[3] := x
    }

    rule q {
        y := Fetch()
    }
}
";
    let (_, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    assert_eq!(group.conflicts.len(), 1);
    assert_eq!(group.conflicts[0].exemption, Exemption::Disjoint);
}

#[test]
fn mem_disjoint_proof_stays_conservative_through_a_callee_param_index() {
    // `Fetch(i) { return m[i] }` called as `Fetch(7)` -- the literal 7
    // lives at the CALL site, not inside `Fetch`'s own body, whose
    // index expression is just `Ident(i)`. `const_index` only folds a
    // bare `Int`/`SizedInt` at the expression itself, never chasing an
    // identifier back through a call's argument substitution -- so this
    // must fail closed and keep the ordinary derived stall, exactly the
    // same restriction the base feature's own doc comment states.
    let src = "\
module M {
    mem m : [8][16]
    in x : [8]
    out y : [8] = 0

    Fetch(i : [4]) : [8] {
        return m[i]
    }

    rule p {
        m[3] := x
    }

    rule q {
        y := Fetch(7)
    }
}
";
    let (_, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    assert_eq!(group.conflicts.len(), 1);
    assert_eq!(group.conflicts[0].exemption, Exemption::None);
}

#[test]
fn mem_disjoint_v2_offsets_wrap_at_the_narrower_of_addr_or_base_width() {
    // A real bug advisor caught before this shipped: `i + k` wraps at
    // the BASE's own declared width, which can be narrower than the
    // mem's address width -- comparing offsets modulo the address width
    // alone is unsound then. Here `m`'s depth is 256 (addr width 8) but
    // `i` is only `[4]` wide: offsets 1 and 17 differ mod 256, but
    // `(i+1) mod 16` and `(i+17) mod 16` are the SAME actual address
    // (17 mod 16 == 1). `.!` is needed here only because `17` doesn't
    // fit `i`'s own `[4]` width otherwise (an unrelated, pre-existing
    // literal-fits check) -- not because of anything this proof does.
    let src = "\
module M {
    mem m : [8][256]
    reg i : [4] = 0
    in x : [8]
    out y : [8] = 0
    rule p {
        m[i +.! 1] := x
    }
    rule q {
        y := m[i +.! 17]
    }
}
";
    let (_, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    assert_eq!(group.conflicts.len(), 1);
    assert_eq!(
        group.conflicts[0].exemption,
        Exemption::None,
        "offsets 1 and 17 alias mod the base's own 4-bit width (17 mod 16 == 1); must stay \
         conservative, not incorrectly proven disjoint"
    );
}

#[test]
fn mem_disjoint_v2_field_base_does_not_recognize_asymmetrically_with_a_bare_ident() {
    // `state_base` requires the `Expr::Ident` shape explicitly: without
    // that, an inst-port `Expr::Field` base (`c.a`) would resolve via
    // `res.expr_defs` just like a bare register would, recognizing
    // `m[c.a + 1]` while a bare `m[c.a]` (the `Expr::Ident`-only match
    // arm in `index_form`) does not -- an accidental asymmetry, not a
    // soundness bug (a port's value is just as pre-edge-stable within a
    // cycle as a register's), but real: this pins that a Field-based
    // index stays unrecognized on BOTH sides, consistently.
    let src = "\
module Child {
    out a : [4] = 0
}
module M {
    mem m : [8][16]
    inst c : Child
    in x : [8]
    out y : [8] = 0
    rule p {
        m[c.a] := x
    }
    rule q {
        y := m[c.a + 1]
    }
}
";
    let (_, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = sched
        .groups
        .iter()
        .find(|g| g.order.len() == 2)
        .expect("M's group");
    assert_eq!(group.conflicts.len(), 1);
    assert_eq!(group.conflicts[0].exemption, Exemption::None);
}

#[test]
fn explain_names_a_proven_disjoint_mem_pair() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/mem_disjoint_rw.tr"
    ))
    .unwrap();
    let (ast, res, sched, errors) = run(&src);
    assert!(errors.is_empty());
    let text = sched.explain(&ast, &res);
    assert!(text.contains("rule write conflicts with rule read"));
    assert!(text.contains(
        "index sites proven disjoint (constant addresses, or the same base plus a constant \
         offset): no stall derived (no annotation needed)"
    ));
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
fn mem_disjoint_constant_indices_clear_a_readwrite_conflict() {
    // DESIGN.md's own "Arrays: one resource each" example (`x := m[i]`
    // vs `m[j] := y`) but with LITERAL, provably-different indices --
    // the scoped v1 auto-proof this closes: no `conflict_free`
    // annotation needed, and the conflict should be gone entirely, not
    // just waived.
    let src = "\
module M {
    mem m : [8][16]
    in x : [8]
    out y : [8] = 0
    rule p {
        m[0] := x
    }
    rule q {
        y := m[1]
    }
}
";
    let (_, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    // Still recorded (same shape as `conflict_free`/`mutually_exclusive`
    // — `--explain-schedule` can say what was proven), but exempted:
    // `is_exempted()` is what firrtl/module.rs actually checks to waive
    // the derived stall, and no user annotation was written at all.
    assert_eq!(group.conflicts.len(), 1);
    assert_eq!(group.conflicts[0].kind, ConflictKind::ReadWrite);
    assert_eq!(group.conflicts[0].exemption, Exemption::Disjoint);
    assert!(group.conflicts[0].exemption.is_exempted());
}

#[test]
fn mem_non_constant_index_stays_conservative() {
    // Same shape, but `q`'s index is a runtime value (an input port),
    // not a literal -- v1 only folds bare integer literals, so this
    // must fail closed and keep the ordinary derived stall, exactly as
    // before this feature existed.
    let src = "\
module M {
    mem m : [8][16]
    in x : [8]
    in i : [4]
    out y : [8] = 0
    rule p {
        m[0] := x
    }
    rule q {
        y := m[i]
    }
}
";
    let (_, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    assert_eq!(group.conflicts.len(), 1);
    assert_eq!(group.conflicts[0].exemption, Exemption::None);
}

#[test]
fn mem_same_constant_index_stays_conservative() {
    // Both sides name the SAME literal address -- must NOT be treated
    // as disjoint just because both are constants.
    let src = "\
module M {
    mem m : [8][16]
    in x : [8]
    out y : [8] = 0
    rule p {
        m[0] := x
    }
    rule q {
        y := m[0]
    }
}
";
    let (_, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    assert_eq!(group.conflicts.len(), 1);
    assert_eq!(group.conflicts[0].exemption, Exemption::None);
}

#[test]
fn mem_writewrite_pair_stays_conservative_even_with_disjoint_literals() {
    // v0 has one shared, priority-muxed write port per mem (see
    // firrtl/module.rs) -- proving two literal addresses disjoint buys
    // nothing there, since a proven-disjoint pair of WRITERS would
    // still race on that one port. The auto-proof must stay scoped to
    // ReadWrite pairs only.
    let src = "\
module M {
    mem m : [8][16]
    in x : [8]
    in z : [8]
    rule p {
        m[0] := x
    }
    rule q {
        m[1] := z
    }
}
";
    let (_, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    assert_eq!(group.conflicts.len(), 1);
    assert_eq!(group.conflicts[0].kind, ConflictKind::WriteWrite);
    assert_eq!(group.conflicts[0].exemption, Exemption::None);
}

#[test]
fn mem_disjoint_proof_does_not_exempt_a_pair_sharing_other_state_too() {
    // `p`/`q` share BOTH a disjoint-proven mem access and an unrelated
    // register -- the mem half alone must not exempt the whole pair;
    // the register conflict is real and still needs the ordinary
    // derived stall.
    let src = "\
module M {
    mem m : [8][16]
    reg shared : [8] = 0
    in x : [8]
    out y : [8] = 0
    out z : [8] = 0
    rule p {
        m[0] := x
        shared := 1
    }
    rule q {
        y := m[1]
        z := shared
    }
}
";
    let (_, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    assert_eq!(group.conflicts.len(), 1);
    assert_eq!(group.conflicts[0].exemption, Exemption::None);
}

#[test]
fn mem_disjoint_v2_same_base_different_offset_is_proven() {
    // `m[i]` vs `m[i+1]`, SAME register `i`, power-of-two depth (16):
    // within one cycle both rules read the identical pre-edge value of
    // `i` (registers are speculatively written, read pre-edge -- the
    // scheduler's own core invariant), so `i` and `i+1` are unconditionally
    // different addresses. No side condition on `i` being written is
    // needed (an earlier, mistaken caution about that got corrected).
    let src = "\
module M {
    mem m : [8][16]
    reg i : [4] = 0
    in x : [8]
    out y : [8] = 0
    rule p {
        m[i] := x
    }
    rule q {
        y := m[i+1]
    }
}
";
    let (_, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    assert_eq!(group.conflicts.len(), 1);
    assert_eq!(group.conflicts[0].exemption, Exemption::Disjoint);
}

#[test]
fn mem_disjoint_v2_same_base_minus_offset_is_proven() {
    // The `Sub` arm: `m[i]` vs `m[i-1]` -- deliberately tested as its own
    // shape, not assumed to fall out of the `Add` arm for free.
    let src = "\
module M {
    mem m : [8][16]
    reg i : [4] = 0
    in x : [8]
    out y : [8] = 0
    rule p {
        m[i] := x
    }
    rule q {
        y := m[i-1]
    }
}
";
    let (_, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    assert_eq!(group.conflicts.len(), 1);
    assert_eq!(group.conflicts[0].exemption, Exemption::Disjoint);
}

#[test]
fn mem_disjoint_v2_commuted_offset_is_proven() {
    // `k + base`, not just `base + k` -- both operand orders must
    // recognize as the same affine shape.
    let src = "\
module M {
    mem m : [8][16]
    reg i : [4] = 0
    in x : [8]
    out y : [8] = 0
    rule p {
        m[i] := x
    }
    rule q {
        y := m[1+i]
    }
}
";
    let (_, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    assert_eq!(group.conflicts.len(), 1);
    assert_eq!(group.conflicts[0].exemption, Exemption::Disjoint);
}

#[test]
fn mem_disjoint_v2_same_base_same_offset_still_conflicts() {
    // `m[i]` vs `m[i]` -- literally the same address every cycle. Being
    // "the same base" must not by itself be treated as license to
    // exempt; the offsets have to differ too.
    let src = "\
module M {
    mem m : [8][16]
    reg i : [4] = 0
    in x : [8]
    out y : [8] = 0
    rule p {
        m[i] := x
    }
    rule q {
        y := m[i]
    }
}
";
    let (_, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    assert_eq!(group.conflicts.len(), 1);
    assert_eq!(group.conflicts[0].exemption, Exemption::None);
}

#[test]
fn mem_disjoint_v2_different_bases_stay_unprovable() {
    // `m[i]` vs `m[j]` for two DISTINCT registers -- this is the case
    // Lumi's own phrasing named directly, and it is deliberately NOT
    // handled: `i` and `j`'s runtime values could coincide, and proving
    // otherwise needs real range tracking, not a syntactic check.
    let src = "\
module M {
    mem m : [8][16]
    reg i : [4] = 0
    reg j : [4] = 0
    in x : [8]
    out y : [8] = 0
    rule p {
        m[i] := x
    }
    rule q {
        y := m[j]
    }
}
";
    let (_, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    assert_eq!(group.conflicts.len(), 1);
    assert_eq!(group.conflicts[0].exemption, Exemption::None);
}

#[test]
fn mem_disjoint_v2_non_power_of_two_depth_stays_unprovable() {
    // Same shape as the proven case above, but depth 10 (not a power of
    // two): `clog2(10) == 4`, so addresses 10..16 are representable but
    // not real cells, and v0 has no bounds check against that at all --
    // an out-of-range address's behavior is undefined, left to firtool.
    // The affine proof must not depend on that undefined behavior, so it
    // simply never fires here.
    let src = "\
module M {
    mem m : [8][10]
    reg i : [4] = 0
    in x : [8]
    out y : [8] = 0
    rule p {
        m[i] := x
    }
    rule q {
        y := m[i+1]
    }
}
";
    let (_, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    assert_eq!(group.conflicts.len(), 1);
    assert_eq!(group.conflicts[0].exemption, Exemption::None);
}

#[test]
fn mem_disjoint_v2_constant_against_affine_stays_unprovable() {
    // `m[3]` vs `m[i]` -- a fixed number says nothing about a variable's
    // possible runtime values, so a constant can never be proven
    // disjoint from an affine form (or the reverse).
    let src = "\
module M {
    mem m : [8][16]
    reg i : [4] = 0
    in x : [8]
    out y : [8] = 0
    rule p {
        m[3] := x
    }
    rule q {
        y := m[i]
    }
}
";
    let (_, _, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    assert_eq!(group.conflicts.len(), 1);
    assert_eq!(group.conflicts[0].exemption, Exemption::None);
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
