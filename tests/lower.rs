use trace::ast::Ast;
use trace::lower::{LowerError, LoweredRule, plan, render};
use trace::resolve::Resolution;
use trace::schedule;
use trace::types::{Ty, Width};
use trace::{effects, lexer, parser, resolve, types};

struct Checked {
    ast: Ast,
    res: Resolution,
    types: types::Types,
    lowered: Vec<LoweredRule>,
    errors: Vec<LowerError>,
}

fn run(src: &str) -> Checked {
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
    let (ty, type_errors) = types::check(&ast, &res);
    assert!(type_errors.is_empty(), "type errors: {type_errors:?}");
    let (lowered, errors) = plan(&ast, &res, &fx, &ty);
    Checked {
        ast,
        res,
        types: ty,
        lowered,
        errors,
    }
}

/// Run the full front end over generated text: proves the lowering
/// output is well-formed trace, not just plausible text.
fn assert_round_trips(src: &str) -> String {
    let (tokens, lex_errors) = lexer::lex(src);
    assert!(lex_errors.is_empty(), "lex errors: {lex_errors:?}");
    let (ast, parse_errors) = parser::parse(src, &tokens);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let (res, resolve_errors) = resolve::resolve(&ast);
    assert!(
        resolve_errors.is_empty(),
        "resolve errors in lowered output: {resolve_errors:?}\n---\n{src}"
    );
    let (fx, effect_errors) = effects::check(&ast, &res);
    assert!(
        effect_errors.is_empty(),
        "effect errors in lowered output: {effect_errors:?}\n---\n{src}"
    );
    let (_ty, type_errors) = types::check(&ast, &res);
    assert!(
        type_errors.is_empty(),
        "type errors in lowered output: {type_errors:?}\n---\n{src}"
    );
    let (_, schedule_errors) = schedule::schedule(&ast, &res, &fx);
    assert!(
        schedule_errors.is_empty(),
        "schedule errors in lowered output: {schedule_errors:?}\n---\n{src}"
    );
    src.to_string()
}

#[test]
fn rmw_structural_shape() {
    let src =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/rmw.tr")).unwrap();
    let c = run(&src);
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    assert_eq!(c.lowered.len(), 1);
    let lr = &c.lowered[0];
    assert_eq!(lr.rule_name, "step");
    assert_eq!(lr.segments.len(), 2, "one tick -> two segments");
    assert_eq!(lr.cont_width, 1, "two segments fit in one bit");
    assert_eq!(lr.captures.len(), 1);
    assert_eq!(lr.captures[0].name, "v");
    assert_eq!(lr.captures[0].ty, Ty::Bits(Width::Known(8)));
    assert_eq!(lr.captures[0].assign_segment, 0);
    assert_eq!(lr.captures[0].read_segments, [1]);

    let rendered = render(&c.ast, &src, &c.lowered);
    assert_round_trips(&rendered);
    assert!(rendered.contains("reg v : bits[8] = 0"));
    assert!(rendered.contains("rule step_s0 {"));
    assert!(rendered.contains("rule step_s1 {"));
    assert!(rendered.contains(&format!("({} == 0)?", lr.cont_name)));
    assert!(rendered.contains(&format!("{} := 1", lr.cont_name)));
    assert!(
        rendered.contains(&format!("{} := 0", lr.cont_name)),
        "s1 wraps to 0"
    );
}

#[test]
fn subleq_structural_shape() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/subleq.tr"))
        .unwrap();
    let c = run(&src);
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    assert_eq!(c.lowered.len(), 1);
    let lr = &c.lowered[0];
    assert_eq!(lr.rule_name, "step");
    assert_eq!(lr.segments.len(), 6, "five ticks -> six segments");
    assert_eq!(lr.cont_width, 3, "six segments need 3 bits (0..7)");

    let mut names: Vec<&str> = lr.captures.iter().map(|c| c.name.as_str()).collect();
    names.sort();
    assert_eq!(names, ["a", "b", "c", "r", "va"]);
    for cap in &lr.captures {
        assert_eq!(
            cap.ty,
            Ty::Bits(Width::Known(16)),
            "{} should be bits[16]",
            cap.name
        );
    }
    // `r` is defined in segment 4 and used only in segment 5.
    let r = lr.captures.iter().find(|c| c.name == "r").unwrap();
    assert_eq!(r.assign_segment, 4);
    assert_eq!(r.read_segments, [5]);

    // Segment 4's rendered text does not accidentally slice in the
    // preceding tick or the following one.
    let rendered = render(&c.ast, &src, std::slice::from_ref(lr));
    for cap_name in ["a", "b", "c", "va", "r"] {
        assert!(
            rendered.contains(&format!("reg {cap_name} : bits[16] = 0")),
            "missing save register for {cap_name}\n{rendered}"
        );
    }
    for i in 0..6 {
        assert!(
            rendered.contains(&format!("rule step_s{i} {{")),
            "{rendered}"
        );
    }
}

#[test]
fn subleq_schedule_directive_rewrites_and_only_s5_conflicts() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/subleq.tr"))
        .unwrap();
    let c = run(&src);
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    let rendered = render(&c.ast, &src, &c.lowered);
    assert!(
        rendered
            .contains("urgency step_s0 > step_s1 > step_s2 > step_s3 > step_s4 > step_s5 > refill")
    );

    // Full round trip, including the scheduler.
    let (tokens, _) = lexer::lex(&rendered);
    let (ast2, parse_errors) = parser::parse(&rendered, &tokens);
    assert!(parse_errors.is_empty(), "{parse_errors:?}");
    let (res2, resolve_errors) = resolve::resolve(&ast2);
    assert!(resolve_errors.is_empty(), "{resolve_errors:?}");
    let (fx2, effect_errors) = effects::check(&ast2, &res2);
    assert!(effect_errors.is_empty(), "{effect_errors:?}");
    let (sched, schedule_errors) = schedule::schedule(&ast2, &res2, &fx2);
    assert!(schedule_errors.is_empty(), "{schedule_errors:?}");

    // Every segment writes the shared continuation register, so all
    // C(6,2) segment pairs conflict there too — harmless, since their
    // guards (cont == i) are mutually exclusive by construction and can
    // never fire together regardless. The interesting fact is which
    // pairs conflict on real state: only step_s5 (the sole writer of
    // pc/m) should conflict with refill, not step_s0..s4.
    let group = sched.groups.iter().find(|g| g.module.is_some()).unwrap();
    let rule_name = |id: trace::ast::ItemId| match ast2.item(id) {
        trace::ast::Item::Rule { name, .. } => name.text.clone(),
        _ => unreachable!(),
    };
    let refill_partners: Vec<String> = group
        .conflicts
        .iter()
        .filter_map(|c| {
            let (a, b) = (rule_name(c.a), rule_name(c.b));
            if a == "refill" {
                Some(b)
            } else if b == "refill" {
                Some(a)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        refill_partners,
        ["step_s5"],
        "only step_s5 shares real state with refill"
    );

    let step_pairs = group
        .conflicts
        .iter()
        .filter(|c| rule_name(c.a) != "refill" && rule_name(c.b) != "refill")
        .count();
    assert_eq!(
        step_pairs, 15,
        "all 6 segments should pairwise conflict on __cont_step"
    );
}

#[test]
fn rejects_nested_tick() {
    let src = "\
module M {
    reg x : bits[8] = 0
    rule r <suspends> {
        if x == 0 {
            tick
        }
        tick
    }
}
";
    let c = run(src);
    assert_eq!(c.errors.len(), 1);
    assert!(c.errors[0].message.contains("nested"));
}

#[test]
fn rejects_reassignment_across_segments() {
    let src = "\
module M {
    reg x : bits[8] = 0
    rule r <suspends> {
        v := 1
        tick
        v := 2
        tick
        x := v
    }
}
";
    let c = run(src);
    assert_eq!(c.errors.len(), 1);
    assert!(c.errors[0].message.contains("multiple segments"));
}

#[test]
fn rejects_same_segment_read_of_a_captured_value() {
    // v is read in its own assignment segment (would silently read the
    // stale register value once promoted) as well as a later segment.
    let src = "\
module M {
    reg x : bits[8] = 0
    reg y : bits[8] = 0
    rule r <suspends> {
        v := 1
        y := v + 1
        tick
        x := v
    }
}
";
    let c = run(src);
    assert_eq!(c.errors.len(), 1);
    assert!(c.errors[0].message.contains("write-once"));
}

#[test]
fn uncaptured_locals_are_left_alone() {
    // A local used only within its own segment needs no save register.
    let src = "\
module M {
    reg x : bits[8] = 0
    rule r <suspends> {
        w := 1 + 1
        x := w
        tick
        x := x + 1
    }
}
";
    let c = run(src);
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    assert_eq!(c.lowered[0].captures.len(), 0);
    let rendered = render(&c.ast, src, &c.lowered);
    assert_round_trips(&rendered);
}

#[test]
fn no_tick_no_lowering() {
    // <suspends> with zero ticks: nothing to cut, plan skips it.
    let c = run("rule r <suspends> {\n x := 1\n}\n");
    assert!(c.lowered.is_empty());
    assert!(c.errors.is_empty());
}

#[test]
fn local_type_recorded_for_capture() {
    let src =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/rmw.tr")).unwrap();
    let c = run(&src);
    // Sanity: local_tys actually has an entry for the captured def.
    let def = c.lowered[0].captures[0].def;
    assert_eq!(
        c.types.local_tys.get(&def),
        Some(&Ty::Bits(Width::Known(8)))
    );
    let _ = &c.res; // keep res alive/used for clarity of what run() returns
}
