use trace::ast::{Ast, Item};
use trace::resolve::Resolution;
use trace::schedule::{ConflictKind, Exemption, Schedule, ScheduleError, schedule};
use trace::{bounds, effects, lexer, parser, resolve, types};

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
    let (b, bounds_errors) = bounds::check(&ast, &res, &fx, &ty);
    assert!(bounds_errors.is_empty(), "bounds errors: {bounds_errors:?}");
    let (sched, errors) = schedule(&ast, &res, &fx, &ty, &b);
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
        "index sites proven disjoint (constant addresses, the same base plus a constant \
         offset, a shared power-of-two multiplier, a proven value bound, or two independently \
         proven disjoint ranges): no stall derived (no annotation needed)"
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
fn mem_disjoint_v3_banked_different_bases_is_proven() {
    // The one shape where two DIFFERENT bases ARE provable: `m[2*i]`
    // (even) against `m[2*j+1]` (odd) can never coincide for ANY i, j --
    // base identity drops out of the argument entirely, since `2*x` is
    // always congruent to 0 mod 2 regardless of which x. This is
    // genuinely different from `mem_disjoint_v2_different_bases_stay_
    // unprovable` just above: same shape (two distinct registers), but a
    // shared power-of-two multiplier makes it provable where a bare
    // shared base offset is not.
    let src = "\
module M {
    mem m : [8][16]
    reg i : [4] = 0
    reg j : [4] = 0
    in x : [8]
    out y : [8] = 0
    rule p {
        m[2*i] := x
    }
    rule q {
        y := m[2*j + 1]
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
fn mem_disjoint_v3_banked_same_parity_stays_unprovable() {
    // The discriminating negative case: `m[2*i]` against `m[2*j+2]` --
    // BOTH even, so the residue argument gives no information (0 mod 2
    // == 2 mod 2), and the two bases are different so the same-base
    // argument doesn't apply either. If this were ever proven disjoint,
    // the modulus would be wrong.
    let src = "\
module M {
    mem m : [8][16]
    reg i : [4] = 0
    reg j : [4] = 0
    in x : [8]
    out y : [8] = 0
    rule p {
        m[2*i] := x
    }
    rule q {
        y := m[2*j + 2]
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
fn mem_disjoint_v3_banked_commuted_multiplier_is_proven() {
    // `i*2` (base first) must recognize identically to `2*i` (base
    // second) -- `IndexForm::mul` commutes when the base is on the
    // right.
    let src = "\
module M {
    mem m : [8][16]
    reg i : [4] = 0
    reg j : [4] = 0
    in x : [8]
    out y : [8] = 0
    rule p {
        m[i*2] := x
    }
    rule q {
        y := m[j*2 + 1]
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
fn mem_disjoint_v4_nested_arithmetic_composes_with_no_new_recognizer_arm() {
    // The whole point of the compositional `IndexForm`/`index_form`
    // refactor: `(i+1)*2` was NOT recognized by the old hardcoded
    // recognizer at all (it only pattern-matched `base+k`, `M*base`,
    // and `M*base+k` at the TOP level, never a `Mul` applied to a
    // nested `Add`) -- confirmed via `git stash` before writing this
    // test, which showed the pre-refactor code falling back to a
    // derived stall here. The new recognizer needs no new arm for this
    // shape: `(i+1)*2` composes to the SAME `{base: i, multiplier: 2,
    // offset: 2}` that `2*i+2` would, via the existing add/mul
    // composition rules alone.
    let src = "\
module M {
    mem m : [8][16]
    reg i : [4] = 0
    reg j : [4] = 0
    in x : [8]
    out y : [8] = 0
    rule p {
        m[(i+1)*2] := x
    }
    rule q {
        y := m[(j+1)*2 + 1]
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
fn mem_disjoint_v4_sum_of_two_bases_stays_unprovable() {
    // The fail-closed direction of the compositional recognizer, not
    // just the positive one above: the OLD recognizer could never even
    // REACH a subexpression with a base on both sides (it only ever
    // looked at specific operand positions via `scaled_base`/
    // `const_index`), but the new one recurses into every operand and
    // relies on `IndexForm::add`'s `(Some, Some) => None` to reject
    // `i + j` (two DIFFERENT bases summed is a genuinely two-variable
    // expression this linear representation can't capture -- silently
    // keeping one base and dropping the other would be a real
    // fail-open miscompile, the same class as the reassigned-local/
    // `Avg(Avg(x,y),z)` bug already shipped and fixed twice).
    let src = "\
module M {
    mem m : [8][16]
    reg i : [4] = 0
    reg j : [4] = 0
    in x : [8]
    out y : [8] = 0
    rule p {
        m[i+j] := x
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
fn mem_disjoint_v4_product_of_two_bases_stays_unprovable() {
    // Same fail-closed direction as the sum case above, for `mul`:
    // `i * j` (both operands a base, neither a constant) must reject via
    // `IndexForm::mul`'s `(Some, Some) => None` -- a product of two
    // unknowns is quadratic, not the linear form this proof reasons
    // about.
    let src = "\
module M {
    mem m : [8][16]
    reg i : [4] = 0
    reg j : [4] = 0
    in x : [8]
    out y : [8] = 0
    rule p {
        m[i*j] := x
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
fn mem_disjoint_v5_proven_bound_closes_a_non_power_of_two_depth() {
    // The actual driving example for `bounds.rs`: `m`'s depth (10) is
    // NOT a power of two, so neither the same-base nor the banking
    // argument can fire (both are gated on `pow2_addr_width`) -- but
    // `i`'s declared bound (`< 9`, proven by `bump`'s own `if i < 8`
    // guard) confines both `m[i+1]` and `m[i]` to the mem's REAL depth
    // (10) directly, no power-of-two padding needed at all.
    let src = "\
module M {
    mem m : [8][10]
    reg i : [4] where i < 9 = 0
    in x : [8]
    out y : [8] = 0
    rule bump {
        if i < 8 {
            i := i + 1
        } else {
            i := 0
        }
    }
    rule wr {
        m[i+1] := x
    }
    rule rd {
        y := m[i]
    }
    schedule {
        conflict_free { bump, wr }
        conflict_free { bump, rd }
    }
}
";
    let (_, res, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    let mem_conflict = group
        .conflicts
        .iter()
        .find(|c| c.on.iter().any(|d| res.def(*d).name == "m"))
        .expect("a conflict on the mem `m` should exist");
    assert_eq!(mem_conflict.exemption, Exemption::Disjoint);
}

#[test]
fn mem_disjoint_v5_same_offset_under_a_proven_bound_stays_unprovable() {
    // `m[i]` vs `m[i]` (identical offset): even with `i`'s bound proven
    // and both sides confined to the mem's real depth, the SAME address
    // is never disjoint from itself -- confirms the proven-bound
    // argument still requires `a.offset != b.offset`, not just "both in
    // range".
    let src = "\
module M {
    mem m : [8][10]
    reg i : [4] where i < 9 = 0
    in x : [8]
    out y : [8] = 0
    rule wr {
        m[i] := x
    }
    rule rd {
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
fn mem_disjoint_v5_a_subtraction_is_now_provable_via_the_stage_4_z3_query() {
    // `m[i-1]` vs `m[i]`, `i : [4] where i < 10`: PROVABLY disjoint under
    // stage 4's Z3 query, unlike the old `IndexForm`/`real_range`-based
    // proof this replaced (which deliberately excluded `Sub` -- see this
    // arc's own git history -- since `IndexForm`'s own WRAPPING
    // arithmetic and `real_range`'s INDEPENDENT non-wrapping walk were
    // two separate representations that could disagree, and treating a
    // wrapped `u64::MAX` offset as a real non-negative integer would
    // have been unsound). The Z3 query has no such mismatch: it
    // translates `i-1` ONCE, at `CALC_WIDTH` (64-bit, non-wrapping), then
    // truncates to the mem's own `addr_width` (4, `clog2(10)`) at the
    // very end -- sound here because `i`'s own declared width (4) is
    // `>= addr_width` (`leaves_wide_enough`, `bounds/smt.rs`'s own doc
    // comment). For every `i` in `[0,10)`, `(i-1) mod 16 != i mod 16`
    // (`i=0`'s own wraparound to 15 included) -- a genuine fact about the
    // two computed ADDRESS bit patterns, independent of whether the mem's
    // depth happens to be a power of two.
    let src = "\
module M {
    mem m : [8][10]
    reg i : [4] where i < 10 = 0
    in x : [8]
    out y : [8] = 0
    rule wr {
        m[i-1] := x
    }
    rule rd {
        y := m[i]
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
fn mem_disjoint_v5_a_subtraction_is_still_sound_when_the_leaf_is_wider_than_addr_width() {
    // The v5 test above has `i`'s own declared width EQUAL to `addr_
    // width` (4 and 4) -- both landing on the same number is not enough
    // to confirm `leaves_wide_enough`'s own gate (`bounds/smt.rs`'s doc
    // comment: sound only when a leaf's width is `>= addr_width`, not
    // `== addr_width`) is checking the right inequality, since equality
    // alone can't distinguish ">=" from "==". Here `i : [8]` (leaf width
    // 8) against the SAME depth-10 mem (`addr_width` still 4, strictly
    // LESS than 8) -- `i` is deliberately left UNBOUNDED (no `where`) so
    // real hardware's own `i-1` truncates at `i`'s FULL 8-bit width
    // (`tail(sub(i,1),1)`, matching `type_binop`'s `max(x,y)` rule) before
    // the mem port's own connect narrows further to 4 bits. At `i=0`:
    // real hardware computes `i-1` as `255` (8-bit wraparound), then the
    // mem address take its low 4 bits, `15`. The Z3 model computes `i-1`
    // at `CALC_WIDTH` (64-bit) and truncates ONCE, at the very end, to 4
    // bits -- `(0-1) mod 2^64`, low 4 bits, ALSO `15`, since 64 >= 8 >= 4
    // (the congruence `bounds/smt.rs`'s own doc comment states holds for
    // any width `>=` the final one). Confirms the gate holds on the
    // STRICT inequality, not merely the equal-widths case the v5 test
    // alone exercises.
    let src = "\
module M {
    mem m : [8][10]
    in i : [8]
    in x : [8]
    out y : [8] = 0
    rule wr {
        m[i-1] := x
    }
    rule rd {
        y := m[i]
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
fn mem_disjoint_v6_disjoint_proven_ranges_across_different_bases() {
    // The actual driving example for the fifth (range-disjointness)
    // argument: `i`'s proven range is `[0,5)`, `j`'s is `[5,10)` -- they
    // never overlap, so `write` (writes `m[i]`) and `read` (reads
    // `m[j]`) are proven disjoint with no shared base, multiplier, or
    // offset relationship at all.
    let src = "\
module M {
    mem m : [8][10]
    reg i : [4] where i < 5 = 0
    reg j : [4] where 5 <= j < 10 = 5
    in x : [8]
    out y : [8] = 0
    rule bump_i {
        if i < 4 {
            i := i + 1
        } else {
            i := 0
        }
    }
    rule bump_j {
        if j < 9 {
            j := j + 1
        } else {
            j := 5
        }
    }
    rule write {
        m[i] := x
    }
    rule read {
        y := m[j]
    }
    schedule {
        conflict_free { bump_i, write }
        conflict_free { bump_j, read }
    }
}
";
    let (_, res, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    let mem_conflict = group
        .conflicts
        .iter()
        .find(|c| c.on.iter().any(|d| res.def(*d).name == "m"))
        .expect("a conflict on the mem `m` should exist");
    assert_eq!(mem_conflict.exemption, Exemption::Disjoint);
}

#[test]
fn mem_disjoint_v6_overlapping_proven_ranges_stay_unprovable() {
    // `i`'s range `[0,5)` and `j`'s range `[3,8)` overlap in `[3,5)` --
    // the range-disjointness argument must not overreach here, and
    // neither base/multiplier is shared, so no other argument can fire
    // either.
    let src = "\
module M {
    mem m : [8][10]
    reg i : [4] where i < 5 = 0
    reg j : [4] where 3 <= j < 8 = 3
    in x : [8]
    out y : [8] = 0
    rule write {
        m[i] := x
    }
    rule read {
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
fn mem_disjoint_v6_constant_vs_bounded_different_base_is_a_freebie() {
    // A bare constant compared against a bounded, unrelated base's index
    // previously always failed closed (`(None, Some(db))` had no
    // argument at all) -- the range-disjointness argument fires here
    // for free, since it's tried before the base-identity match: `3` is
    // outside `j`'s proven range `[5,10)`.
    let src = "\
module M {
    mem m : [8][10]
    reg j : [4] where 5 <= j < 10 = 5
    in x : [8]
    out y : [8] = 0
    rule write {
        m[3] := x
    }
    rule read {
        y := m[j]
    }
}
";
    let (_, res, sched, errors) = run(src);
    assert!(errors.is_empty());
    let group = &sched.groups[0];
    let mem_conflict = group
        .conflicts
        .iter()
        .find(|c| c.on.iter().any(|d| res.def(*d).name == "m"))
        .expect("a conflict on the mem `m` should exist");
    assert_eq!(mem_conflict.exemption, Exemption::Disjoint);
}

#[test]
fn mem_disjoint_v7_mirrored_odd_offset_is_proven() {
    // `examples/mirrored_index_disjoint.tr`'s own driving case: `m[i]`
    // vs `m[7 - i]`, SAME base `i`. `2*i (mod 16)` is always even, and
    // `7` is odd, so there is NO `i` for which `i = 7 - i (mod 16)` --
    // provable with no power-of-two DEPTH requirement at all (unlike the
    // v1/v3 arguments), since the argument only needs the base's own
    // width to be a power of two, which every bit-width already is.
    //
    // Naming note: this file's `v6`/`v7` test-name prefixes already
    // diverge from `forms_differ`'s own doc comment, which calls the
    // range-disjointness argument the "FIFTH" and this one the "SIXTH"
    // -- the test prefix counts shipped FEATURES chronologically (`v6`
    // covers what the doc calls the fourth AND fifth arguments, shipped
    // together at v16), the doc comment counts independent PROOF
    // ARGUMENTS. Both are internally consistent; they just don't share
    // one counter.
    let src = "\
module M {
    mem m : [8][8]
    reg i : [4] where i < 8 = 0
    in x : [8]
    out y : [8] = 0
    rule p {
        m[i] := x
    }
    rule q {
        y := m[7 - i]
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
fn mem_disjoint_v7_mirrored_even_offset_stays_unprovable() {
    // The discriminating negative case, and the one that actually proves
    // the `negated`-flag guard on the OTHER arguments is load-bearing,
    // not just cautious: `m[i]` vs `m[2 - i]` genuinely COLLIDES at
    // `i = 1` (`2 - 1 = 1`), a real runtime aliasing this proof must NOT
    // claim disjoint. Before the `same_base_and_multiplier` check
    // required `a.negated == b.negated`, this exact pair was wrongly
    // proven disjoint (confirmed by temporarily removing the guard
    // during development): `i`'s offset (0) and `2 - i`'s offset (2)
    // differ mod 8, which the pre-existing same-base argument reads as
    // "same shape, offsets differ" -- true for two ADDING forms, but
    // false here, since one side's base term is negated and the two
    // don't cancel in the subtraction the way the argument assumes.
    let src = "\
module M {
    mem m : [8][8]
    reg i : [4] where i < 8 = 0
    in x : [8]
    out y : [8] = 0
    rule p {
        m[i] := x
    }
    rule q {
        y := m[2 - i]
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
fn mem_disjoint_v7_mirrored_even_offset_stays_unprovable_under_a_narrow_bound() {
    // A SECOND, differently-shaped discriminating negative case (an
    // advisor follow-up, since the test above uses `i < 8` -- wide
    // enough that `i`'s own range `[0,8)` and `2 - i`'s computed range
    // overlap so broadly it doesn't discriminate the RANGE-based
    // arguments specifically): here `i < 2` is narrow enough that
    // `bounds.rs`'s own `Sub` arm proves a real range for `2 - i` too
    // (`[1,3)`), which could in principle let the FOURTH/FIFTH
    // (range-disjointness) arguments fire independently of the mirrored
    // one -- confirming they don't, since `i`'s range `[0,2)` and
    // `2 - i`'s range `[1,3)` genuinely overlap at the real collision
    // point (`i = 1`, `2 - 1 = 1`). See `forms_differ`'s own doc comment
    // for why a SOUND range can never be disjoint at a point it actually
    // achieves.
    let src = "\
module M {
    mem m : [8][10]
    reg i : [4] where i < 2 = 0
    in x : [8]
    out y : [8] = 0
    rule p {
        m[i] := x
    }
    rule q {
        y := m[2 - i]
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
fn mem_disjoint_v7_mirrored_different_bases_stays_unprovable() {
    // The mirrored argument requires the SAME base on both sides (it
    // reasons about `2*base`, a single-variable equation) -- `m[i]` vs
    // `m[7 - j]` for two DIFFERENT registers stays exactly as unprovable
    // as `mem_disjoint_v2_different_bases_stay_unprovable`, regardless
    // of the odd offset: `i` and `j` could still coincide with `7 - j`.
    let src = "\
module M {
    mem m : [8][8]
    reg i : [4] where i < 8 = 0
    reg j : [4] where j < 8 = 0
    in x : [8]
    out y : [8] = 0
    rule p {
        m[i] := x
    }
    rule q {
        y := m[7 - j]
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
fn mem_disjoint_v3_banked_non_power_of_two_multiplier_stays_unprovable() {
    // `3*i` isn't a power of two, so the banking argument's `M.is_
    // power_of_two()` guard rejects it outright -- and since the bases
    // are also different, the same-base argument doesn't apply either.
    // Both conditions block this pair (unlike the depth test above,
    // which isolates a single failing condition): this test only
    // confirms neither argument fires, not which one specifically would
    // if the other's guard were removed.
    let src = "\
module M {
    mem m : [8][16]
    reg i : [5] = 0
    reg j : [5] = 0
    in x : [8]
    out y : [8] = 0
    rule p {
        m[3*i] := x
    }
    rule q {
        y := m[3*j + 1]
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
fn mem_disjoint_v2_non_power_of_two_depth_is_now_provable_via_the_stage_4_z3_query() {
    // Same shape as the proven case above, but depth 10 (not a power of
    // two): `clog2(10) == 4`, so addresses 10..16 are representable but
    // not real cells, an out-of-range address's behavior undefined, left
    // to firtool. The OLD `forms_differ`/`IndexForm` affine argument
    // (this arc's own git history) required the depth to be an EXACT
    // power of two before it would even attempt the same-base argument,
    // so this stayed unprovable under it. Stage 4's Z3 query has no such
    // requirement: `i` and `i+1` are provably different ADDRESS bit
    // patterns (`i mod 16 != (i+1) mod 16` for every `i`, including the
    // `i=15` wraparound to `0`) regardless of what the mem's real depth
    // is -- the proof is purely about the two computed addresses never
    // coinciding, not about whether either address happens to be a real
    // cell or padding.
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
    assert_eq!(group.conflicts[0].exemption, Exemption::Disjoint);
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

#[test]
fn mem_site_narrowing_schedules_with_no_stall() {
    // v16's own driving example: `write`/`read` each narrow a
    // DIFFERENT reg (`i`/`j`, both merely declared `< 20`, individually
    // insufficient) via their OWN `if` guard to a disjoint half of the
    // mem's depth. `bounds.rs`'s newly-exported per-site fact is what
    // lets `schedule.rs` prove this automatically (no annotation) --
    // without it, this same file (confirmed via a pre-fix scratch run)
    // schedules with a real STALL (`read` waits on `write`), not a
    // compile error, since there's no `conflict_free` claim to fall
    // back to trusting either.
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/mem_site_narrowing.tr"
    ))
    .unwrap();
    let (_, _, sched, errors) = run(&src);
    assert!(errors.is_empty(), "{errors:?}");
    let group = &sched.groups[0];
    assert_eq!(group.conflicts.len(), 1);
    assert_eq!(group.conflicts[0].exemption, Exemption::Disjoint);
}

#[test]
fn mem_read_derived_value_does_not_feed_the_disjointness_proof() {
    // v17's own regression guard for the soundness hole an advisor pass
    // caught before the mem-elem-bound feature shipped: an earlier
    // version of `expr_bound`'s `Bracket` arm handed a mem's declared
    // elem bound back at READ sites too, which meant `let a = m[pc]`
    // (composing `m`'s declared range onto `a`) then `m[a]` would export
    // a FABRICATED range into `Bounds.site_ranges` -- exactly the shape
    // `subleq.tr` itself has (an index loaded out of the mem). Chosen so
    // the bug would have produced a FALSE proof, not just a missed one:
    // `m`'s elem bound (`[10, 20)`) and the write index `pc`'s own range
    // (`[0, 10)`) are DISJOINT, so if the buggy version were still
    // shipping, `a` would carry `[10, 20)` and `forms_differ`'s
    // non-overlapping-ranges argument would prove `write`/`read`
    // disjoint -- a real false claim about synthesized hardware, since
    // `a` is actually whatever garbage `m[pc]` returns, not a value
    // provably confined to `[10, 20)`. Post-fix, `a` is a plain
    // `Local` with no bound at all, so `real_range` misses and the pair
    // falls back to the ordinary derived stall.
    let src = "\
module M {
    mem m : [8][20] where 10 <= _ < 20
    reg pc : [8] where pc < 10 = 0
    reg total : [8] = 0
    rule write {
        m[pc] := 15
    }
    rule read {
        let a = m[pc]
        total := m[a]
    }
}
";
    let (_, _, sched, errors) = run(src);
    assert!(errors.is_empty(), "{errors:?}");
    let group = &sched.groups[0];
    assert_eq!(group.conflicts.len(), 1);
    assert_eq!(group.conflicts[0].exemption, Exemption::None);
}

// --- The `schedule.rs` consumer half of DESIGN.md's "Tier 3, not v0"
// circular-buffer case (`bounds::provably_disjoint_under_joint_
// guards`) ---

#[test]
fn circular_buffer_mem_drops_from_the_conflict_set_but_stall_persists() {
    // The real driving example, verbatim: `push`/`pop` share `{m,
    // push_count, pop_count}`. `m`'s own index proof (`m[head]` vs
    // `m[tail]`, via the two declared `invariant`s and both rules' own
    // joint guards) now succeeds, dropping `m` from the reported `on`
    // set -- but `push_count`/`pop_count` are ordinary scalar regs, not
    // mem, so the pair's overall exemption stays `None` and the derived
    // stall persists exactly as before. This is the accounting DESIGN
    // .md documents: the mem-disjointness proof's entire visible payoff
    // on this example is `m` dropping out of the diagnostic, not the
    // conflict itself clearing.
    let src = "\
module CircularBufferDisjoint {
    mem m : [8][8]
    reg head : [4] where head < 8 = 0
    reg tail : [4] where tail < 8 = 0
    reg push_count : [4] = 0
    reg pop_count : [4] = 0
    invariant push_count - pop_count < 9
    invariant (head - tail - push_count + pop_count) % 8 = 0
    in push_en : [1]
    in push_data : [8]
    in pop_en : [1]
    out pop_data : [8] = 0
    rule push {
        (push_en = 1)?
        (push_count - pop_count < 8)?
        m[head] := push_data
        if head < 7 {
            head := head + 1
        } else {
            head := 0
        }
        push_count := push_count + 1
    }
    rule pop {
        (pop_en = 1)?
        (push_count <> pop_count)?
        pop_data := m[tail]
        if tail < 7 {
            tail := tail + 1
        } else {
            tail := 0
        }
        pop_count := pop_count + 1
    }
}
";
    let (ast, res, sched, errors) = run(src);
    assert!(errors.is_empty(), "{errors:?}");
    let group = &sched.groups[0];
    assert_eq!(group.conflicts.len(), 1);
    let conflict = &group.conflicts[0];
    assert_eq!(conflict.exemption, Exemption::None);
    let on: std::collections::BTreeSet<&str> = conflict
        .on
        .iter()
        .map(|d| res.def(*d).name.as_str())
        .collect();
    assert_eq!(
        on,
        ["push_count", "pop_count"].into_iter().collect(),
        "`m` should have dropped out of the reported set"
    );
    assert_eq!(
        rule_name(&ast, conflict.winner),
        "push",
        "declaration order still decides the winner"
    );
}

#[test]
fn circular_buffer_weakened_guard_never_drops_m_from_the_conflict_set() {
    // The load-bearing negative, at the `schedule.rs` layer: with
    // `push`'s guard weakened to admit occupancy 8, the occupancy
    // invariant itself fails to verify in `bounds.rs` (a compile
    // error), so `bounds.relational` never gets the fact this proof
    // needs -- `m` stays in the reported conflict set exactly as it did
    // before this feature existed.
    let src = "\
module CircularBufferDisjoint {
    mem m : [8][8]
    reg head : [4] where head < 8 = 0
    reg tail : [4] where tail < 8 = 0
    reg push_count : [4] = 0
    reg pop_count : [4] = 0
    invariant push_count - pop_count < 9
    invariant (head - tail - push_count + pop_count) % 8 = 0
    in push_en : [1]
    in push_data : [8]
    in pop_en : [1]
    out pop_data : [8] = 0
    rule push {
        (push_en = 1)?
        (push_count - pop_count <= 8)?
        m[head] := push_data
        if head < 7 {
            head := head + 1
        } else {
            head := 0
        }
        push_count := push_count + 1
    }
    rule pop {
        (pop_en = 1)?
        (push_count <> pop_count)?
        pop_data := m[tail]
        if tail < 7 {
            tail := tail + 1
        } else {
            tail := 0
        }
        pop_count := pop_count + 1
    }
}
";
    let (tokens, lex_errors) = lexer::lex(src);
    assert!(lex_errors.is_empty());
    let (ast, parse_errors) = parser::parse(src, &tokens);
    assert!(parse_errors.is_empty());
    let (res, resolve_errors) = resolve::resolve(&ast);
    assert!(resolve_errors.is_empty());
    let (fx, effect_errors) = effects::check(&ast, &res);
    assert!(effect_errors.is_empty());
    let (ty, type_errors) = types::check(&ast, &res, &fx);
    assert!(type_errors.is_empty());
    let (b, bounds_errors) = bounds::check(&ast, &res, &fx, &ty);
    assert_eq!(bounds_errors.len(), 1); // the occupancy invariant fails to verify
    let (sched, sched_errors) = schedule(&ast, &res, &fx, &ty, &b);
    assert!(sched_errors.is_empty());
    let group = &sched.groups[0];
    assert_eq!(group.conflicts.len(), 1);
    let on: std::collections::BTreeSet<&str> = group.conflicts[0]
        .on
        .iter()
        .map(|d| res.def(*d).name.as_str())
        .collect();
    assert_eq!(
        on,
        ["m", "push_count", "pop_count"].into_iter().collect(),
        "`m` should NOT drop out -- the occupancy invariant never verified"
    );
}
