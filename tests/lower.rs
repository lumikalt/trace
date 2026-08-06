use trace::ast::Ast;
use trace::lower::{LowerError, LoweredRule, plan, render};
use trace::resolve::Resolution;
use trace::schedule;
use trace::types::{Ty, Width};
use trace::{bounds, effects, lexer, parser, resolve, types};

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
    let (ty, type_errors) = types::check(&ast, &res, &fx);
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
    let (ty, type_errors) = types::check(&ast, &res, &fx);
    assert!(
        type_errors.is_empty(),
        "type errors in lowered output: {type_errors:?}\n---\n{src}"
    );
    let (b, bounds_errors) = bounds::check(&ast, &res, &fx, &ty);
    assert!(
        bounds_errors.is_empty(),
        "bounds errors in lowered output: {bounds_errors:?}\n---\n{src}"
    );
    let (_, schedule_errors) = schedule::schedule(&ast, &res, &fx, &ty, &b);
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
    assert!(rendered.contains("reg v : [8] = 0"));
    assert!(rendered.contains("rule step_s0 {"));
    assert!(rendered.contains("rule step_s1 {"));
    assert!(rendered.contains(&format!("({} = 0)?", lr.cont_name)));
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
            "{} should be [16]",
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
            rendered.contains(&format!("reg {cap_name} : [16] = 0")),
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
    let (ty2, type_errors) = types::check(&ast2, &res2, &fx2);
    assert!(type_errors.is_empty(), "{type_errors:?}");
    let (b2, bounds_errors) = bounds::check(&ast2, &res2, &fx2, &ty2);
    assert!(bounds_errors.is_empty(), "{bounds_errors:?}");
    let (sched, schedule_errors) = schedule::schedule(&ast2, &res2, &fx2, &ty2, &b2);
    assert!(schedule_errors.is_empty(), "{schedule_errors:?}");

    // Every segment writes the shared continuation register, so all
    // C(6,2) segment pairs conflict there too — harmless, since their
    // guards (cont = i) are mutually exclusive by construction and can
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
    reg x : [8] = 0
    rule r <sequences> {
        if x = 0 {
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
    reg x : [8] = 0
    rule r <sequences> {
        let v = 1
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
    reg x : [8] = 0
    reg y : [8] = 0
    rule r <sequences> {
        let v = 1
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

/// `while`'s own segment-cutting (DESIGN.md's "`while`: multi-cycle
/// loops"): a top-level `while` gets its own dedicated segment,
/// `while_cond` set, plus the ordinary trailing segment for whatever
/// follows — three segments total for one `while` with code before and
/// after it. `cont_width` picks up the extra segments automatically
/// (`clog2` over the new total), not hand-maintained.
#[test]
fn while_loop_structural_shape() {
    let src = "\
module M {
    in x : [8]
    out result : [8] = 0
    reg cnt : [8] = 0

    rule r <sequences, fails> {
        cnt := x
        while cnt <> 0 {
            cnt := cnt - 1
        }
        result := cnt
    }
}
";
    let c = run(src);
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    assert_eq!(c.lowered.len(), 1);
    let lr = &c.lowered[0];
    assert_eq!(
        lr.segments.len(),
        3,
        "pre-while, the while itself, post-while"
    );
    assert!(!lr.segments[0].is_while_loop);
    assert!(lr.segments[1].is_while_loop);
    assert!(!lr.segments[2].is_while_loop);
    assert_eq!(lr.cont_width, 2, "3 segments need 2 bits");

    let rendered = render(&c.ast, src, &c.lowered);
    assert_round_trips(&rendered);
    assert!(rendered.contains("if cnt <> 0 {"));
    assert!(rendered.contains(&format!("{} := 1\n    }} else {{", lr.cont_name)));
    assert!(rendered.contains(&format!("{} := 2\n    }}", lr.cont_name)));
}

/// A local written inside a `while` loop's own segment and needed
/// elsewhere — an accumulator (`acc := acc + n`), or anything else
/// surviving past the loop — is a clean v0 rejection naming the real
/// cause, not the generic write-once capture text (`rejects_
/// reassignment_across_segments`'s message would read as nonsense here:
/// there's no OTHER segment reassigning `acc`, just the loop's own one,
/// executed every iteration). `while` may only write module state
/// directly this pass.
#[test]
fn while_loop_local_accumulator_across_iterations_is_rejected() {
    let src = "\
module M {
    in x : [8]
    out result : [8] = 0

    rule r <sequences, fails> {
        let acc = 0
        let n = x
        while n <> 0 {
            acc := acc + n
            n := n - 1
        }
        result := acc
    }
}
";
    let c = run(src);
    assert_eq!(c.errors.len(), 2, "{:?}", c.errors);
    assert!(
        c.errors
            .iter()
            .all(|e| e.message.contains("across a `while` loop boundary"))
    );
}

/// `while` must sit at a sequences rule's top level, not nested inside
/// `if`/another `while` — the same v0 restriction `tick`/`spawn` already
/// have, and for the same reason: `split_into_segments` only ever looks
/// at the top level, so a nested `while` would otherwise silently fold
/// into whichever segment it landed in as ordinary, un-lowered text.
#[test]
fn while_nested_in_if_is_rejected() {
    let src = "\
module M {
    reg cnt : [8] = 0
    reg flag : [1] = 0

    rule r <sequences, fails> {
        if flag = 1 {
            while cnt <> 0 {
                cnt := cnt - 1
            }
        }
    }
}
";
    let c = run(src);
    assert_eq!(c.errors.len(), 1);
    assert!(c.errors[0].message.contains("top level"));
    assert!(c.errors[0].message.contains("nested in if/while"));
}

/// `while let`'s own segment-cutting (DESIGN.md's "`while`: multi-cycle
/// loops"): identical structural shape to plain `while`'s own
/// (`while_loop_structural_shape` above) — `while let` cuts a dedicated
/// segment the same way, `is_while_loop` set the same way. What's
/// DIFFERENT is render time: `while_loop_header` renders `if let NAME =
/// EXPR { ... }` instead of `if COND { ... }`, reusing `if let`'s own
/// already-tested emission machinery wholesale rather than adding any.
#[test]
fn while_let_loop_structural_shape() {
    let src = "\
module M {
    in x : [8]
    out result : [8] = 0
    reg opt : ?[8] = false

    rule r <sequences, fails> {
        opt := x
        while let v = opt? {
            result := v
            opt := false
        }
    }
}
";
    let c = run(src);
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    assert_eq!(c.lowered.len(), 1);
    let lr = &c.lowered[0];
    assert_eq!(
        lr.segments.len(),
        3,
        "pre-while, the while let itself, post-while"
    );
    assert!(!lr.segments[0].is_while_loop);
    assert!(lr.segments[1].is_while_loop);
    assert!(!lr.segments[2].is_while_loop);

    let rendered = render(&c.ast, src, &c.lowered);
    assert_round_trips(&rendered);
    assert!(rendered.contains("if let v = opt? {"));
    assert!(rendered.contains(&format!("{} := 1\n    }} else {{", lr.cont_name)));
    assert!(rendered.contains(&format!("{} := 2\n    }}", lr.cont_name)));
}

/// `while let`'s own bound name inherits `if let`'s v0 restrictions
/// verbatim — a local written inside the loop and needed elsewhere is
/// rejected the same way `while`'s own arm is, since `while let`'s loop
/// segment is just another segment `compute_captures` treats no
/// differently from a plain `while` segment.
#[test]
fn while_let_loop_local_accumulator_across_iterations_is_rejected() {
    let src = "\
module M {
    reg opt : ?[8] = false
    out result : [8] = 0

    rule r <sequences, fails> {
        let acc = 0
        while let v = opt? {
            acc := acc + v
            opt := false
        }
        result := acc
    }
}
";
    let c = run(src);
    assert_eq!(c.errors.len(), 1, "{:?}", c.errors);
    assert!(
        c.errors[0]
            .message
            .contains("across a `while` loop boundary")
    );
}

/// `while let` must sit at a top level too, the identical restriction
/// plain `while` has, checked by the SAME `find_nested_while` (folded
/// into one function rather than a separate `find_nested_while_let`).
#[test]
fn while_let_nested_in_if_is_rejected() {
    let src = "\
module M {
    reg opt : ?[8] = false
    reg flag : [1] = 0
    out result : [8] = 0

    rule r <sequences, fails> {
        if flag = 1 {
            while let v = opt? {
                result := v
                opt := false
            }
        }
    }
}
";
    let c = run(src);
    assert_eq!(c.errors.len(), 1);
    assert!(c.errors[0].message.contains("top level"));
    assert!(c.errors[0].message.contains("nested in if/while"));
}

#[test]
fn uncaptured_locals_are_left_alone() {
    // A local used only within its own segment needs no save register.
    let src = "\
module M {
    reg x : [8] = 0
    rule r <sequences> {
        let w = 1 + 1
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
fn let_bound_value_crossing_a_tick_now_works() {
    // `let v = value` used to rely on `render`'s splice-verbatim trick,
    // which only worked for `x := value` (still a plain register write
    // once `x` becomes a `reg`) -- `let x = value` always bound a FRESH
    // local instead, shadowing the register rather than writing it, so
    // this was rejected outright (see git history). Once `let` became
    // the ONLY way to declare a fresh local (resolve.rs, see TODO.md),
    // `render_rule` learned to rewrite a `let`-bound capture's own
    // declaring statement into `{name} := ` before splicing
    // (`CapturedLocal::let_prefix_span`), so this now works correctly
    // instead of needing a dedicated rejection.
    let src = "\
module M {
    out out : [8] = 0
    rule r <sequences> {
        let v = 8'd5
        tick
        out := v + 1
    }
}
";
    let c = run(src);
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    assert_eq!(c.lowered[0].captures.len(), 1);
    let rendered = render(&c.ast, src, &c.lowered);
    assert!(rendered.contains("reg v : [8] = 0"));
    assert!(rendered.contains("v := 8'd5"));
    assert_round_trips(&rendered);
}

#[test]
fn let_bound_value_crossing_a_tick_in_a_spawn_callee_now_works() {
    // Same fix applies inside a spawned <sequences> fn's own body,
    // since `plan_spawn` reuses the identical `compute_captures`
    // machinery as a top-level rule -- the rewrite there targets the
    // RENAMED `__save_...` register instead of the plain original name,
    // since a spawn callee's captures always go through the rename
    // scheme.
    let src = "\
Foo() : [8] <sequences> {
    let v = 8'd5
    tick
    return v + 1
}

module M {
    out out : [8] = 0
    rule r <sequences> {
        let h = spawn Foo()
        tick sync[h]
        out := h.result
    }
}
";
    let c = run(src);
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    let rendered = render(&c.ast, src, &c.lowered);
    assert!(rendered.contains(" := 8'd5"));
    assert_round_trips(&rendered);
}

#[test]
fn let_bound_value_within_one_segment_is_unaffected() {
    // A `let` that never needs to cross a tick stays an ordinary local
    // -- only a `let` that must survive past a tick is rejected.
    let src = "\
module M {
    out out : [8] = 0
    rule r <sequences> {
        let v = 8'd5
        out := v + 1
        tick
    }
}
";
    let c = run(src);
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    assert_eq!(c.lowered[0].captures.len(), 0);
}

#[test]
fn shadowed_let_captures_of_the_same_name_are_rejected() {
    // Two DISTINCT `let x` bindings (different DefIds, one shadowing the
    // other) that both cross a tick would otherwise both become captures
    // named "x" -- `render_rule` would emit two `reg x : [8] = 0`
    // lines, producing lowered output that fails to re-resolve rather
    // than a clean error at plan time.
    let src = "\
module M {
    in a : [8]
    in b : [8]
    out out1 : [8] = 0
    out out2 : [8] = 0
    rule r <sequences> {
        let x = a
        tick
        out1 := x
        let x = b
        tick
        out2 := x
    }
}
";
    let c = run(src);
    assert_eq!(c.errors.len(), 1);
    assert!(c.errors[0].message.contains("shadows"));
}

#[test]
fn spawn_bound_with_let_now_works() {
    // `spawn_trigger_shape` used to recognize only `Stmt::Assign` (`h :=
    // spawn ...`); a `let`-bound spawn fell through to the generic
    // unsupported-construct scan and got blamed on `race`. Now that
    // `let` is the ONLY way to declare a fresh local, `spawn_trigger_
    // shape` recognizes `Stmt::Let` directly -- and needs no render-side
    // rewrite at all, since `render_spawn_trigger` always synthesizes
    // brand-new register-write lines from the extracted handle rather
    // than splicing the trigger statement's own text.
    let src = "\
Foo() : [8] <sequences> {
    tick
    return 8'd1
}

module M {
    out out : [8] = 0
    rule r <sequences> {
        let h = spawn Foo()
        tick sync[h]
        out := h.result
    }
}
";
    let c = run(src);
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    assert_eq!(c.lowered[0].spawns.len(), 1);
}

#[test]
fn no_tick_no_lowering() {
    // <sequences> with zero ticks: nothing to cut, plan skips it.
    let c = run("rule r <sequences> {\n let x = 1\n}\n");
    assert!(c.lowered.is_empty());
    assert!(c.errors.is_empty());
}

const FETCH2: &str = "\
module Fetch2 {
    mem bank0 : [16][8]
    mem bank1 : [16][8]
    in pc : [16]
    out ir : [32] = 0

    ReadBank0(addr : [16]) : [16] <sequences> {
        let v = bank0[addr]
        tick
        return v
    }

    ReadBank1(addr : [16]) : [16] <sequences> {
        let v = bank1[addr]
        tick
        return v
    }

    rule fetch2 <sequences> {
        let h1 = spawn ReadBank0(pc)
        let h2 = spawn ReadBank1(pc + 1)
        tick sync[h1, h2]
        ir := pack(h1.result, h2.result)
    }
}
";

#[test]
fn spawn_sync_structural_shape() {
    let c = run(FETCH2);
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    assert_eq!(c.lowered.len(), 1);
    let lr = &c.lowered[0];
    assert_eq!(lr.segments.len(), 2, "one tick -> two segments");
    assert_eq!(lr.spawns.len(), 2);
    assert_eq!(lr.syncs.len(), 1);
    assert!(
        lr.captures.is_empty(),
        "h1/h2 are handles, not ordinary captured locals: {:?}",
        lr.captures
    );

    for spawn in &lr.spawns {
        assert_eq!(spawn.segments.len(), 2, "ReadBank's own tick -> 2 segments");
        assert_eq!(spawn.args.len(), 1);
        assert_eq!(spawn.captures.len(), 1, "ReadBank's own `v` is captured");
        assert_eq!(spawn.captures[0].name, "v");
        assert_eq!(spawn.result_ty, Ty::Bits(Width::Known(16)));
    }

    let rendered = render(&c.ast, FETCH2, &c.lowered);
    let rendered = assert_round_trips(&rendered);
    assert!(rendered.contains("__cont_fetch2_h1 : [1] = 0"));
    assert!(rendered.contains("__cont_fetch2_h2 : [1] = 0"));
    assert!(rendered.contains("__arg_fetch2_h1_addr := pc"));
    assert!(rendered.contains("__done_fetch2_h1 = 1"));
    assert!(rendered.contains("__done_fetch2_h2 = 1"));
    assert!(rendered.contains("pack(__result_fetch2_h1, __result_fetch2_h2)"));
    assert!(
        !rendered.contains("spawn"),
        "no `spawn` keyword should survive lowering"
    );
    assert!(
        !rendered.contains("sync["),
        "no `sync` call should survive lowering"
    );
}

#[test]
fn spawn_nested_in_if_is_rejected() {
    let src = "\
Slow(x : [8]) : [8] <sequences> {
    tick
    return x
}

module M {
    rule r <sequences> {
        (1 = 1)?
        if 1 = 1 {
            let h = spawn Slow(1)
        }
        tick
    }
}
";
    let c = run(src);
    assert_eq!(c.errors.len(), 1);
    assert!(c.errors[0].message.contains("top level"));
}

#[test]
fn sync_nested_in_if_is_rejected() {
    let src = "\
Slow(x : [8]) : [8] <sequences> {
    tick
    return x
}

module M {
    rule r <sequences> {
        let h = spawn Slow(1)
        tick
        if 1 = 1 {
            sync[h]
        }
    }
}
";
    let c = run(src);
    assert_eq!(c.errors.len(), 1);
    assert!(c.errors[0].message.contains("v0 restriction"));
}

#[test]
fn race_value_form_lowers() {
    // `out := race[...]` (assign-shaped, into an EXISTING output rather
    // than a fresh local -- the still-recognized backward-compat shape,
    // see `race_value_bound_with_let_now_works` for the ordinary `let`
    // form into a fresh local) IS the value-producing form -- no
    // leading `tick` is structurally required (it's the segment's own
    // guard, same as a bare `race[...]` statement never needed one
    // either), though `tick out := race[...]` is the idiomatic spelling.
    let src = "\
Slow(x : [8]) : [8] <sequences> {
    tick
    return x
}

module M {
    out out : [8] = 0
    rule r <sequences> {
        let h1 = spawn Slow(1)
        let h2 = spawn Slow(2)
        tick
        out := race[h1, h2]
    }
}
";
    let c = run(src);
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    assert_eq!(c.lowered[0].races.len(), 1);
}

#[test]
fn race_bracket_used_elsewhere_is_still_rejected() {
    // Only the two recognized statement shapes (bare `race[...]`,
    // `w := race[...]`) lower -- embedded in a larger expression still
    // falls through to the generic unsupported-construct message.
    let src = "\
Slow(x : [8]) : [8] <sequences> {
    tick
    return x
}

module M {
    rule r <sequences> {
        let h1 = spawn Slow(1)
        let h2 = spawn Slow(2)
        tick
        let w = race[h1, h2] + 1
    }
}
";
    let c = run(src);
    assert_eq!(c.errors.len(), 1);
    assert!(c.errors[0].message.contains("`race`"));
}

#[test]
fn race_value_bound_with_let_now_works() {
    let src = "\
Slow(x : [8]) : [8] <sequences> {
    tick
    return x
}

module M {
    out out : [8] = 0
    rule r <sequences> {
        let h1 = spawn Slow(1)
        let h2 = spawn Slow(2)
        tick
        let w = race[h1, h2]
        out := w
    }
}
";
    let c = run(src);
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    let rendered = render(&c.ast, src, &c.lowered);
    assert!(rendered.contains("let w = __race_value("));
    assert_round_trips(&rendered);
}

#[test]
fn race_nested_in_if_is_rejected() {
    let src = "\
Slow(x : [8]) : [8] <sequences> {
    tick
    return x
}

module M {
    rule r <sequences> {
        let h1 = spawn Slow(1)
        let h2 = spawn Slow(2)
        tick
        if 1 = 1 {
            race[h1, h2]
        }
    }
}
";
    let c = run(src);
    assert_eq!(c.errors.len(), 1);
    assert!(c.errors[0].message.contains("v0 restriction"));
}

#[test]
fn race_structural_shape() {
    let src = "\
Fast(x : [8]) : [8] <sequences> {
    tick
    return x + 1
}
Slow(x : [8]) : [8] <sequences> {
    tick
    tick
    return x + 2
}

module M {
    out out : [8] = 0
    rule pick <sequences> {
        let hf = spawn Fast(1)
        let hs = spawn Slow(1)
        tick
        race[hf, hs]
        if hf.done = 1 {
            out := hf.result
        } else {
            out := hs.result
        }
    }
}
";
    let c = run(src);
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    assert_eq!(c.lowered.len(), 1);
    let lr = &c.lowered[0];
    assert_eq!(lr.spawns.len(), 2);
    assert_eq!(lr.races.len(), 1);
    assert_eq!(lr.races[0].1.len(), 2);

    let rendered = render(&c.ast, src, &c.lowered);
    let rendered = assert_round_trips(&rendered);
    // Race's own guard: an OR of both handles' done registers.
    assert!(rendered.contains("__done_pick_hf | __done_pick_hs) = 1"));
    // Every one of the losing side's OWN segments gains an extra guard
    // requiring the OTHER handle hasn't already finished -- checked on
    // BOTH spawns' segments, not just one, since either could lose.
    assert!(rendered.contains("__cont_pick_hf = 0"));
    assert!(rendered.contains("__done_pick_hs = 0"));
    assert!(rendered.contains("__cont_pick_hs = 0"));
    assert!(rendered.contains("__done_pick_hf = 0"));
    assert!(
        !rendered.contains("race["),
        "no `race` call should survive lowering"
    );
}

#[test]
fn spawning_the_same_handle_twice_is_rejected() {
    // `let h = spawn Slow(1)` declares the handle; a SECOND `h := spawn
    // ...` (now a plain reassignment of that already-`let`-bound `h`,
    // per `spawn_trigger_shape`'s `Stmt::Assign` arm) reuses the SAME
    // handle_def rather than getting its own -- each spawn occurrence
    // needs its own private register set, so this must be caught
    // directly here, not left to surface as a confusing "already
    // defined" resolve error several passes later on an auto-generated
    // register name.
    let src = "\
Slow(x : [8]) : [8] <sequences> {
    tick
    return x
}

module M {
    rule r <sequences> {
        let h = spawn Slow(1)
        h := spawn Slow(2)
        tick
    }
}
";
    let c = run(src);
    assert_eq!(c.errors.len(), 1);
    assert!(
        c.errors[0]
            .message
            .contains("already names an earlier `spawn`")
    );
}

// A wrong argument count is caught earlier, by types.rs's ordinary call
// arity check on `spawn`'s inner call expression (same as any other
// call) — `run()` here always sees type-checked input, so there's no
// reachable case to test at this layer; `plan_spawn`'s own arity check
// stays as defense-in-depth for `lower::plan`'s public API, which
// doesn't statically force a caller to have checked types first.

#[test]
fn spawn_callee_early_return_is_rejected() {
    let src = "\
Slow(x : [8]) : [8] <sequences> {
    if x = 0 {
        return x
    }
    tick
    return x
}

module M {
    rule r <sequences> {
        let h = spawn Slow(1)
        tick
    }
}
";
    let c = run(src);
    assert_eq!(c.errors.len(), 1);
    assert!(c.errors[0].message.contains("early return"));
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

fn rule_body(ast: &Ast, name: &str) -> Vec<trace::ast::StmtId> {
    for item in &ast.items {
        if let trace::ast::Item::Rule {
            name: rule_name,
            body,
            ..
        } = item
            && rule_name.text == name
        {
            return body.clone();
        }
    }
    panic!("no rule named `{name}` in this program");
}

#[test]
fn sequences_cycle_count_is_the_segment_count_for_a_straight_line_body() {
    // examples/rmw.tr's own driving case: one `tick` -> two segments ->
    // two cycles, TODO.md's own "cost opacity" gap made a checked,
    // queryable quantity instead of invisible everywhere in the
    // compiler.
    let src =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/rmw.tr")).unwrap();
    let c = run(&src);
    let body = rule_body(&c.ast, "step");
    assert_eq!(trace::lower::sequences_cycle_count(&c.ast, &body), Some(2));
}

#[test]
fn sequences_cycle_count_is_none_for_a_while_loop() {
    // A `while` loop's own trip count is a runtime value, not a static
    // one -- must fail closed to `None`, not guess a number.
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/while_countdown.tr"
    ))
    .unwrap();
    let c = run(&src);
    let body = rule_body(&c.ast, "r");
    assert_eq!(trace::lower::sequences_cycle_count(&c.ast, &body), None);
}

#[test]
fn sequences_cycle_count_is_none_when_the_body_spawns() {
    // A `spawn`'d callee's own duration isn't known to the CALLER's
    // segment-splitting at all -- waiting on it via `sync`/`race` makes
    // the caller's own duration data-dependent too, same as `while`.
    let src = "\
Slow(x : [8]) : [8] <sequences> {
    tick
    return x
}

module M {
    rule r <sequences> {
        let h = spawn Slow(1)
        tick
        let v = sync[h]
        tick
    }
}
";
    let c = run(src);
    let body = rule_body(&c.ast, "r");
    assert_eq!(trace::lower::sequences_cycle_count(&c.ast, &body), None);
}
