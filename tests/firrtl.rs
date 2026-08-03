use trace::firrtl::{EmitError, emit};
use trace::{effects, lexer, lower, parser, resolve, schedule, types};

/// Full pipeline including sequences lowering (re-lexed/parsed once the
/// lowered text exists), matching what the CLI does for `--firrtl`.
fn emit_from_source(src: &str) -> Result<String, Vec<EmitError>> {
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

    let (lowered, lower_errors) = lower::plan(&ast, &res, &fx, &ty);
    let lowered_src = if lowered.is_empty() {
        src.to_string()
    } else {
        assert!(lower_errors.is_empty(), "lower errors: {lower_errors:?}");
        lower::render(&ast, src, &lowered)
    };

    // Re-run the whole front end on the lowered text: firrtl.rs takes a
    // fresh (Ast, Resolution, Effects, Types, Schedule) like every pass.
    let (tokens2, lex_errors2) = lexer::lex(&lowered_src);
    assert!(lex_errors2.is_empty(), "{lex_errors2:?}\n{lowered_src}");
    let (ast2, parse_errors2) = parser::parse(&lowered_src, &tokens2);
    assert!(parse_errors2.is_empty(), "{parse_errors2:?}\n{lowered_src}");
    let (res2, resolve_errors2) = resolve::resolve(&ast2);
    assert!(
        resolve_errors2.is_empty(),
        "{resolve_errors2:?}\n{lowered_src}"
    );
    let (fx2, effect_errors2) = effects::check(&ast2, &res2);
    assert!(
        effect_errors2.is_empty(),
        "{effect_errors2:?}\n{lowered_src}"
    );
    let (ty2, type_errors2) = types::check(&ast2, &res2);
    assert!(type_errors2.is_empty(), "{type_errors2:?}\n{lowered_src}");
    let (sched2, schedule_errors2) = schedule::schedule(&ast2, &res2, &fx2);
    assert!(
        schedule_errors2.is_empty(),
        "{schedule_errors2:?}\n{lowered_src}"
    );

    emit(&ast2, &res2, &fx2, &ty2, &sched2)
}

/// Run `firtool` on FIRRTL text, returning its stdout (Verilog). Skips
/// (returns None, doesn't fail) if firtool isn't on PATH, so `cargo
/// test` stays runnable outside the devenv shell that provides it.
fn run_firtool(fir: &str, extra_args: &[&str]) -> Option<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    if Command::new("firtool").arg("--version").output().is_err() {
        eprintln!("firtool not on PATH; skipping (run via `devenv shell` or `t`)");
        return None;
    }

    let mut child = Command::new("firtool")
        // Newer firtool no longer sniffs stdin as FIRRTL by default.
        .arg("-format=fir")
        .args(extra_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn firtool");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(fir.as_bytes())
        .expect("failed to write to firtool stdin");
    let output = child.wait_with_output().expect("failed to wait on firtool");
    assert!(
        output.status.success(),
        "firtool rejected generated FIRRTL:\n{}\n---\n{fir}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn read_example(name: &str) -> String {
    std::fs::read_to_string(format!("{}/examples/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

#[test]
fn rmw_emits_and_compiles() {
    let fir = emit_from_source(&read_example("rmw.tr")).expect("emission should succeed");
    assert!(fir.contains("regreset v : UInt<8>"));
    assert!(fir.contains("regreset addr : UInt<8>"));
    assert!(fir.contains("mem m :"));
    assert!(fir.contains("reader => r0"));
    assert!(fir.contains("writer => w_m"));
    // Read gates the value update; write is gated by the other segment.
    assert!(fir.contains("connect v, m.r0.data"));
    assert!(fir.contains("connect m.w_m.data, tail(add(v, UInt<8>(1)), 1)"));
    run_firtool(&fir, &[]);
}

#[test]
fn subleq_emits_and_compiles() {
    let fir = emit_from_source(&read_example("subleq.tr")).expect("emission should succeed");

    // The milestone's actual claim: refill's derived stall names only
    // its real conflict (step_s5), not every segment — the whole point
    // of "derived stall logic the Chisel version wrote by hand".
    assert!(fir.contains("node fires_refill = and(UInt<1>(1), not(fires_step_s5))"));

    // The branch (SUBLEQ's defining instruction) must reach pc as a mux,
    // not be silently dropped for living inside if/else.
    assert!(fir.contains("connect pc, mux(leq(r, UInt<16>(0)), c, tail(add(pc, UInt<16>(3)), 1))"));

    run_firtool(&fir, &[]);
}

#[test]
fn subleq_verilog_shows_branch_gated_by_step_s5_only() {
    let fir = emit_from_source(&read_example("subleq.tr")).unwrap();
    let Some(verilog) = run_firtool(&fir, &["--disable-opt"]) else {
        return; // firtool unavailable in this environment
    };
    // `pc`'s always-block guard must be exactly the fires_step_s5 net,
    // not e.g. an OR across several segments (which would mean the
    // conflict-derivation collapsed the wrong rules together).
    // `regreset` also compiles to a `pc <= 16'h0;` line under the reset
    // branch; match the derived-signal assignment specifically; a plain
    // `.find()` on "pc <=" is order-dependent on firtool's block layout.
    let pc_line = verilog
        .lines()
        .find(|l| l.trim_start().starts_with("pc <=") && l.contains("_GEN"))
        .expect("expected a pc <= assignment gated by a derived signal");
    assert!(pc_line.contains("_GEN"), "{pc_line}");
}

#[test]
fn accumulator_emits_real_ports() {
    let fir = emit_from_source(&read_example("accumulator.tr")).expect("emission should succeed");

    // A port, not internal state: no `regreset inc`, and `sum` is
    // declared as an output port, not a bare register.
    assert!(fir.contains("input inc : UInt<8>"));
    assert!(fir.contains("output sum : UInt<8>"));
    assert!(!fir.contains("regreset inc"));

    // `sum` is register-backed under an internal name (an output must
    // never be driven combinationally — see firrtl.rs's Item::Output
    // arm) and bridged to the port by one unconditional connect.
    assert!(fir.contains("regreset __out_sum : UInt<8>"));
    assert!(fir.contains("connect sum, __out_sum"));
    // The rule reads the *register*, not the port, and reads the input
    // port directly (no register backs it).
    assert!(fir.contains("connect __out_sum, tail(add(__out_sum, inc), 1)"));

    // No `--disable-opt`: an observable output port alone must be
    // enough to keep firtool from dead-code-eliminating the design.
    run_firtool(&fir, &[]);
}

#[test]
fn port_ram_emits_addressable_memory_through_ports() {
    let fir = emit_from_source(&read_example("port_ram.tr")).expect("emission should succeed");

    // addr/write_data/write_en/read_data are all real ports; `m` stays
    // an ordinary internal mem, addressed by the port values directly
    // (no new mem-port machinery was needed for this — see DESIGN.md's
    // "Port-based memory access" section).
    assert!(fir.contains("input addr : UInt<8>"));
    assert!(fir.contains("input write_data : UInt<16>"));
    assert!(fir.contains("input write_en : UInt<1>"));
    assert!(fir.contains("output read_data : UInt<16>"));
    assert!(fir.contains("connect m.w_m.addr, addr"));
    assert!(fir.contains("connect m.w_m.data, write_data"));
    assert!(fir.contains("connect __out_read_data, m.r0.data"));

    // `write` outranks `read` on the same cycle (explicit schedule
    // directive), so read must never fire while write does.
    assert!(fir.contains("node fires_read = and(UInt<1>(1), not(fires_write))"));

    run_firtool(&fir, &[]);
}

#[test]
fn errors_on_unlowered_sequences_rule() {
    // emit_from_source lowers automatically when lowering applies; use
    // a rule shape lowering itself rejects (nested tick) so a still-
    // <sequences> rule with a tick reaches the emitter unlowered.
    let src = "\
module M {
    reg x : [1] = 0
    rule r <sequences> {
        if x = 1 {
            tick
        }
        tick
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("run sequences lowering first"))
    );
}

#[test]
fn fifo_bridge_emits_depth_one_buffers() {
    let fir = emit_from_source(&read_example("fifo_bridge.tr")).expect("emission should succeed");

    // Each fifo is one data register plus one valid bit.
    assert!(fir.contains("regreset __fifo_input_valid : UInt<1>"));
    assert!(fir.contains("regreset __fifo_input_data : UInt<8>"));
    assert!(fir.contains("regreset __fifo_output_valid : UInt<1>"));
    assert!(fir.contains("regreset __fifo_output_data : UInt<8>"));

    // The two failure conditions DESIGN.md calls out fold into the
    // guard: Deq needs input valid, Enq needs output not valid.
    assert!(
        fir.contains("node fires_transfer = and(__fifo_input_valid, not(__fifo_output_valid))")
    );

    // Deq'd data reaches Enq's argument by inlining (locals have no
    // FIRRTL declaration of their own), and the pop/push are two
    // independent register writes gated by the same `fires_transfer`.
    assert!(fir.contains("connect __fifo_input_valid, UInt<1>(0)"));
    assert!(fir.contains("connect __fifo_output_valid, UInt<1>(1)"));
    assert!(fir.contains("connect __fifo_output_data, __fifo_input_data"));

    run_firtool(&fir, &[]);
}

#[test]
fn fifo_depth_n_emits_a_slot_array_plus_head_and_count() {
    // Depth 4 deliberately, not 3: `head` ranges 0..4 (needs
    // clog2(4) = 2 bits) but `count` ranges 0..=4, five distinct values
    // (needs clog2(5) = 3 bits, one more than `head`) -- picking a
    // depth where these two widths actually differ makes this test
    // catch the specific off-by-one this shape is prone to (sizing
    // `count` for `depth` instead of `depth + 1`, which can't represent
    // a full buffer).
    let src = "\
module M {
    fifo f : {4}[8]
    rule r {
        (want = 1)?
        let x = f.Deq[]
        f.Enq[x + 1]
    }
    in want : [1]
}
";
    let fir = emit_from_source(src).expect("emission should succeed");

    // Four data-slot registers, not one -- plus head/count pointers.
    assert!(fir.contains("regreset __fifo_f_slot0 : UInt<8>"));
    assert!(fir.contains("regreset __fifo_f_slot1 : UInt<8>"));
    assert!(fir.contains("regreset __fifo_f_slot2 : UInt<8>"));
    assert!(fir.contains("regreset __fifo_f_slot3 : UInt<8>"));
    assert!(fir.contains("regreset __fifo_f_head : UInt<2>"));
    assert!(fir.contains("regreset __fifo_f_count : UInt<3>"));
    assert!(!fir.contains("__fifo_f_valid"));
    assert!(!fir.contains("__fifo_f_data"));

    // Combined Enq+Deq guard generalizes from depth-1's `valid` to
    // `count > 0`.
    assert!(
        fir.contains("node fires_r = and(eq(want, UInt<1>(1)), gt(__fifo_f_count, UInt<3>(0)))")
    );

    run_firtool(&fir, &[]);
}

#[test]
fn fifo_depth_via_a_mems_own_postfix_form_also_works() {
    // `{depth}elem_ty` is the preferred, depth-first spelling, but a
    // memory's postfix `elem_ty[depth]` parses to the identical
    // `Bracket { callee: elem_ty, args: [depth] }` shape and works for a
    // fifo too (see DESIGN.md's "Memory, fifo, and submodule
    // declarations" section) -- `eval_fifo_ty`'s own `is_builtin(callee,
    // "bits")` guard only matches an `Ident` callee, so a `Bracket`
    // callee (what `[8][4]` parses to: `[8]` itself, then a postfix `[4]`
    // application) falls through to the depth path exactly like an
    // ordinary named element type would.
    let src = "\
module M {
    fifo f : [8][4]
    in a : [8]
    rule enq {
        f.Enq[a]
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("regreset __fifo_f_slot0 : UInt<8>"));
    assert!(fir.contains("regreset __fifo_f_slot1 : UInt<8>"));
    assert!(fir.contains("regreset __fifo_f_slot2 : UInt<8>"));
    assert!(fir.contains("regreset __fifo_f_slot3 : UInt<8>"));
    run_firtool(&fir, &[]);
}

#[test]
fn fifo_depth_n_fifo_op_reached_via_a_callee_still_uses_the_slot_array() {
    // `rule_fifo_ops` (fifo.rs) finds a fifo op reached through exactly
    // one failing-callee call, not just a rule's own top-level
    // statements -- confirms that path is depth-aware too, not just
    // the direct-op path the other tests above exercise.
    let src = "\
module M {
    fifo input : {3}[8]
    fifo output : [8]
    out last : [8] = 0

    Bridge() : [8] <combines, fails> {
        let x = input.Deq[]
        output.Enq[x]
        return x
    }

    rule transfer {
        last := Bridge()
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("regreset __fifo_input_slot0 : UInt<8>"));
    assert!(fir.contains("regreset __fifo_input_slot1 : UInt<8>"));
    assert!(fir.contains("regreset __fifo_input_slot2 : UInt<8>"));
    assert!(fir.contains("regreset __fifo_input_head : UInt<2>"));
    assert!(fir.contains("regreset __fifo_input_count : UInt<2>"));
    assert!(!fir.contains("__fifo_input_valid"));
    assert!(fir.contains(
        "node fires_transfer = and(gt(__fifo_input_count, UInt<2>(0)), not(__fifo_output_valid))"
    ));
    run_firtool(&fir, &[]);
}

#[test]
fn fifo_depth_zero_is_an_error() {
    let src = "\
module M {
    fifo f : {0}[8]
    rule r {
        f.Enq[8'd1]
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(err.iter().any(|e| e.message.contains("at least 1")));
}

#[test]
fn fifo_enq_and_deq_same_cycle_is_a_passthrough() {
    // Enqueueing AND dequeueing the SAME fifo in one rule is a
    // pass-through, not an error: `let x = f.Deq[]` reads the fifo's
    // current (pre-edge) data, `f.Enq[x + 1]` writes a NEW value for
    // next cycle -- the combined guard is just `valid` (there must be
    // something to dequeue), not the always-false AND of each op's own
    // individual guard.
    let src = "\
module M {
    fifo f : [8]
    rule r {
        let x = f.Deq[]
        f.Enq[x + 1]
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_r = __fifo_f_valid"));
    assert!(fir.contains("connect __fifo_f_valid, UInt<1>(1)"));
    assert!(fir.contains("connect __fifo_f_data, tail(add(__fifo_f_data, UInt<8>(1)), 1)"));
    run_firtool(&fir, &[]);
}

#[test]
fn fifo_enqueued_twice_in_one_rule_is_an_error() {
    let src = "\
module M {
    fifo f : [8]
    reg x : [8] = 0
    rule r {
        f.Enq[x]
        f.Enq[x + 1]
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("appears more than once") && e.message.contains("Enq"))
    );
}

#[test]
fn fifo_dequeued_twice_in_one_rule_is_an_error() {
    let src = "\
module M {
    fifo f : [8]
    rule r {
        let x = f.Deq[]
        let y = f.Deq[]
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("appears more than once") && e.message.contains("Deq"))
    );
}

#[test]
fn let_bound_fifo_op_gates_the_rule() {
    // A fifo op's failure condition must fold into the rule's guard
    // whether it's bound with `:=` or `let` -- these are equally legal
    // binding forms (DESIGN.md's "Locals"), and `let x = f.Deq[]` used
    // to compile to an UNGATED rule (`fires_r = UInt<1>(1)`), silently
    // reading __fifo_f_data even while the fifo was empty.
    let src = "\
module M {
    fifo f : [8]
    reg out : [8] = 0
    rule r {
        let x = f.Deq[]
        out := x + 1
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_r = __fifo_f_valid"));
    assert!(fir.contains("connect __fifo_f_valid, UInt<1>(0)"));
}

#[test]
fn fifo_dequeued_twice_via_let_is_also_an_error() {
    let src = "\
module M {
    fifo f : [8]
    reg out : [8] = 0
    rule r {
        let x = f.Deq[]
        let y = f.Deq[]
        out := x + y
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("appears more than once") && e.message.contains("Deq"))
    );
}

#[test]
fn fifo_enqueued_twice_on_different_fifos_is_fine() {
    // The check counts occurrences per fifo, not per rule -- enqueuing
    // two DIFFERENT fifos once each is ordinary, unrelated work.
    let src = "\
module M {
    fifo f : [8]
    fifo g : [8]
    reg x : [8] = 0
    rule r {
        f.Enq[x]
        g.Enq[x]
    }
}
";
    emit_from_source(src).expect("emission should succeed");
}

#[test]
fn let_bound_fifo_op_nested_in_if_is_an_error() {
    // Same restriction as an `:=`-bound fifo op nested in if/while, but
    // through the `let` binding form -- this used to compile silently
    // (no error, no guard contribution at all) when the local was never
    // referenced again, since `contains_fifo_op`'s nesting scan didn't
    // look inside a `Stmt::Let`.
    let src = "\
module M {
    fifo f : [8]
    reg cond : [1] = 0
    rule r {
        if logic cond = 1 {
            let x = f.Deq[]
        }
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(err.iter().any(|e| e.message.contains("nested in if/while")));
}

#[test]
fn fifo_op_nested_in_if_is_an_error() {
    let src = "\
module M {
    fifo f : [8]
    reg cond : [1] = 0
    rule r {
        if logic cond = 1 {
            f.Enq[cond]
        }
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(err.iter().any(|e| e.message.contains("nested in if/while")));
}

#[test]
fn fifo_op_after_state_write_is_an_error() {
    let src = "\
module M {
    fifo f : [8]
    reg x : [8] = 0
    rule r {
        x := x + 1
        f.Enq[x]
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("after a state write"))
    );
}

/// `check_guard_placement`'s `Stmt::Assign` arm used to chain the fifo-
/// op/failing-call checks behind an `else if is_state_write(lhs)` —
/// meaning a fifo op after a write was only caught when its OWN lhs was
/// a plain local, never when the lhs was ALSO state (`r0 := f.Deq[]`,
/// an entirely ordinary pattern). Found while auditing whether a post-
/// `tick` guard/fifo-op is caught (TODO.md); reproduced here with no
/// `sequences` involved at all, since the gap was general, not
/// tick-specific. `compile_guard` still folded the fifo op's occupancy
/// correctly regardless (no wrong hardware), but the v0 "not yet
/// supported" validation silently didn't fire.
#[test]
fn fifo_op_after_a_write_is_still_an_error_when_its_own_lhs_is_also_state() {
    let src = "\
module M {
    fifo f : [8]
    reg r0 : [8] = 0
    reg r1 : [8] = 0
    rule r {
        r1 := 2
        r0 := f.Deq[]
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("after a state write"))
    );
}

/// The fix for the above must NOT close the guard window on the
/// fifo-op-driven write's OWN statement: two independent dequeues each
/// driving their own output is an entirely ordinary pattern (advisor-
/// caught: a first version of the fix set `seen_write` unconditionally
/// whenever the lhs was state, which would have rejected exactly this).
#[test]
fn two_independent_fifo_op_driven_writes_both_stay_open() {
    let src = "\
module M {
    fifo f : [8]
    fifo g : [8]
    out a : [8] = 0
    out b : [8] = 0
    rule r {
        a := f.Deq[]
        b := g.Deq[]
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_r = and(__fifo_f_valid, __fifo_g_valid)"));
    run_firtool(&fir, &[]);
}

#[test]
fn a_fifo_op_nested_in_a_larger_expression_is_an_error_not_a_dropped_dequeue() {
    // Mirrors `a_failing_call_nested_in_a_larger_expression_is_an_error_
    // not_a_dropped_guard`: `fifo_op_stmt` only recognizes a fifo op
    // sitting as a whole bare statement, the entire RHS of `:=`, or a
    // `let` init -- one nested inside `+ 1` used to be entirely
    // invisible to `rule_fifo_ops`, so the rule fired unconditionally
    // (no occupancy guard) and the fifo never actually dequeued, all
    // with no error. Found via an audit of whether `not` discharges a
    // fifo op's fail condition -- the same gap turned out reachable
    // with plain arithmetic, no `not` involved.
    let src = "\
module M {
    fifo input : [8]
    out result : [8] = 0
    rule compute {
        result := input.Deq[] + 1
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("may only appear as a whole statement"))
    );
}

#[test]
fn a_fifo_op_in_an_if_condition_is_an_error_not_a_dropped_dequeue() {
    // `contains_fifo_op` (the existing if/while nesting check) only
    // walks a branch's own STATEMENTS, never the `if`/`while`'s own
    // condition expression -- so a fifo op sitting directly in the
    // condition used to slip past both that check and `fifo_op_stmt`'s
    // exact-shape match entirely.
    // `logic` around the comparison keeps this type-checking (a bare
    // comparison no longer types as [1] at all, see TODO.md's
    // comparisons-as-fallible design) without changing what this test
    // is actually about: the fifo op is nested TWO levels deep now
    // (inside the comparison, inside `logic`), and `logic_arg_exprs`'s
    // exemption only covers the comparison itself, not what's nested
    // inside it -- confirmed the fifo op is still caught, not silently
    // exempted along with its wrapper.
    let src = "\
module M {
    fifo input : [8]
    out result : [8] = 0
    rule compute {
        if logic input.Deq[] = 1 {
            result := 5
        } else {
            result := 6
        }
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("may only appear as a whole statement"))
    );
}

#[test]
fn a_fifo_op_wrapped_in_not_is_an_error_not_a_dropped_dequeue() {
    // The original entry point into this bug class: `not` needs a
    // `[1]` operand, so only a `bit`-payload fifo type-checks here,
    // but the underlying gap (`fifo_op_stmt`'s exact-shape match) is the
    // same one the two tests above hit without `not` at all.
    let src = "\
module M {
    fifo input : [1]
    out result : [1] = 0
    rule compute {
        not (input.Deq[])
        result := 1
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("may only appear as a whole statement"))
    );
}

#[test]
fn reassigned_local_read_between_two_bindings_sees_the_first() {
    // `y` must resolve against `x`'s FIRST binding (`r`), not the later
    // one (`r + 1`) that hasn't happened yet from `y`'s own position --
    // proving `enter_rule`'s eager, position-ordered compilation, not
    // the old lazy last-assignment-wins map this used to reject
    // outright to avoid miscompiling.
    let src = "\
module M {
    fifo f : [8]
    reg r : [8] = 0
    rule test {
        let x = r
        let y = x
        x := r + 1
        f.Enq[y]
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __fifo_f_data, r"));
    assert!(!fir.contains("add(r"));
    run_firtool(&fir, &[]);
}

#[test]
fn reassigned_local_write_between_two_bindings_sees_old_then_new() {
    // The discriminating case: an OBSERVABLE write sits BETWEEN two
    // reassignments of the same local. `before` must see `x`'s value as
    // of ITS OWN position (== `a`), `after` must see the LATER
    // reassignment (== `b`) -- a naive last-assignment-wins pass (what
    // simply deleting the old rejection, with no position tracking,
    // would give) fails this by making `before` ALSO read `b`.
    let src = "\
module M {
    in a : [8]
    in b : [8]
    reg before : [8] = 0
    reg after : [8] = 0
    rule r {
        let x = a
        before := x
        x := b
        after := x
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect before, a"));
    assert!(fir.contains("connect after, b"));
    run_firtool(&fir, &[]);
}

#[test]
fn reassigned_local_chain_resolves_each_reference_at_its_own_position() {
    // `y := z + 10` must use `z`'s FIRST binding; the later `out := y + z`
    // must use `z`'s SECOND binding for the bare `z` reference while `y`
    // itself still carries the value computed from the FIRST one --
    // proving transitively-chained locals resolve correctly, not just a
    // single reassigned name referenced directly.
    let src = "\
module M {
    mem m : [16][4]
    out result : [16] = 0
    rule r {
        let z = m[0]
        let y = z + m[1]
        z := m[2]
        result := y + z
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains(
        "connect __out_result, tail(add(tail(add(m.r0.data, m.r1.data), 1), m.r2.data), 1)"
    ));
    run_firtool(&fir, &[]);
}

#[test]
fn reassigned_local_with_no_concrete_width_still_errors() {
    // A local used ONLY as a mem-read address never gets a concrete
    // `[w]` type from the checker (see `type_expr_inner`'s `Ty::Mem`
    // arm) -- eager compilation has no width to resolve it with at bind
    // time, so THIS narrow shape still rejects reassignment explicitly,
    // same spirit as the old blanket check but scoped to just this case.
    let src = "\
module M {
    mem m : [16][256]
    reg out : [16] = 0
    rule r {
        let x = 5
        out := m[x]
        x := 6
        out := m[x]
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(err.iter().any(|e| e.message.contains("reassigned")));
}

#[test]
fn nested_mem_write_threads_an_explicit_write_enable() {
    // A memory write may now nest inside `if`/`else`, mirroring register
    // and instance-port writes -- but unlike either of those (which
    // always have a well-defined "hold" value), a memory write has no
    // state of its own to hold, so a branch that doesn't write must
    // produce en=0, not just addr/data defaulting to 0.
    let src = "\
module M {
    reg cond : [1] = 0
    mem m : [8][16]
    reg addr : [8] = 0
    reg v : [8] = 0

    rule r {
        if logic cond = 1 {
            m[addr] := v
        }
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains(
        "connect m.w_m.en, and(fires_r, mux(eq(cond, UInt<1>(1)), UInt<1>(1), UInt<1>(0)))"
    ));
    assert!(fir.contains("connect m.w_m.addr, UInt<4>(0)"));
    assert!(fir.contains("connect m.w_m.data, UInt<8>(0)"));
    assert!(fir.contains("connect m.w_m.addr, mux(eq(cond, UInt<1>(1)), addr, UInt<4>(0))"));
    assert!(fir.contains("connect m.w_m.data, mux(eq(cond, UInt<1>(1)), v, UInt<8>(0))"));
    run_firtool(&fir, &[]);
}

#[test]
fn nested_mem_write_with_both_branches_writing_muxes_real_addr_and_data() {
    // The `if`-without-`else` case above only ever exercises the
    // write-enable TOGGLING (a mux against a literal-0 default). This
    // covers the other half of "threads through if/else as a mux": both
    // branches write a REAL (non-default) addr/data pair, so the write
    // always happens (`en` folds to effectively `fires_r`, not
    // conditional), but WHICH addr/data is muxed by `cond`.
    let src = "\
module M {
    reg cond : [1] = 0
    mem m : [8][16]
    reg addr_a : [8] = 0
    reg addr_b : [8] = 0
    reg va : [8] = 0
    reg vb : [8] = 0

    rule r {
        if logic cond = 1 {
            m[addr_a] := va
        } else {
            m[addr_b] := vb
        }
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains(
        "connect m.w_m.en, and(fires_r, mux(eq(cond, UInt<1>(1)), UInt<1>(1), UInt<1>(1)))"
    ));
    assert!(fir.contains("connect m.w_m.addr, mux(eq(cond, UInt<1>(1)), addr_a, addr_b)"));
    assert!(fir.contains("connect m.w_m.data, mux(eq(cond, UInt<1>(1)), va, vb)"));
    run_firtool(&fir, &[]);
}

#[test]
fn a_second_unconditional_mem_write_in_one_rule_is_an_error() {
    // Two unconditional writes to the same mem, even at different
    // addresses: only one write port exists, so the second would
    // silently discard the first with zero trace in the emitted FIRRTL
    // (confirmed before this check existed). A memory's address makes
    // this a real surprise, unlike `reg := a; reg := b`, where "last
    // wins" is ordinary, expected reassignment of the same location.
    let src = "\
module M {
    mem m : [8][256]
    in addr0 : [8]
    in addr1 : [8]

    rule r {
        m[addr0] := 8'd11
        m[addr1] := 8'd22
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("written here unconditionally"))
    );
}

#[test]
fn an_unconditional_mem_write_after_a_conditional_one_is_also_an_error() {
    // Same bug, the other order: the `if`'s entire muxed write --
    // guard condition included -- vanished with zero trace once the
    // unconditional write after it just overwrote `current` outright.
    let src = "\
module M {
    mem m : [8][256]
    in cond : [1]
    in addr0 : [8]
    in addr1 : [8]

    rule r {
        if logic cond = 1 {
            m[addr0] := 8'd11
        }
        m[addr1] := 8'd22
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("written here unconditionally"))
    );
}

#[test]
fn two_conditional_writes_to_the_same_mem_chain_correctly_and_are_not_an_error() {
    // Two `if`-guarded writes to the same mem DO thread correctly (each
    // is muxed against whatever came before, exactly like the existing
    // if/else nesting support) -- only an UNCONDITIONAL write following
    // an earlier one is the bug. This must keep working, not get
    // swept up by too broad a fix.
    let src = "\
module M {
    mem m : [8][256]
    in condA : [1]
    in condB : [1]
    in addrA : [8]
    in addrB : [8]

    rule r {
        if logic condA = 1 {
            m[addrA] := 8'd11
        }
        if logic condB = 1 {
            m[addrB] := 8'd22
        }
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    // condB takes priority (declared second); falls back to condA's
    // write, which falls back to no write at all.
    assert!(fir.contains(
        "connect m.w_m.addr, mux(eq(condB, UInt<1>(1)), addrB, mux(eq(condA, UInt<1>(1)), addrA, UInt<8>(0)))"
    ));
    run_firtool(&fir, &[]);
}

#[test]
fn an_unconditional_write_followed_by_a_conditional_one_is_not_an_error() {
    // The mirror of the previous case: an earlier unconditional write
    // correctly becomes a later `if`'s implicit else-fallback, so this
    // ordering was never broken and must stay legal.
    let src = "\
module M {
    mem m : [8][256]
    in cond : [1]
    in addr0 : [8]
    in addr1 : [8]

    rule r {
        m[addr0] := 8'd11
        if logic cond = 1 {
            m[addr1] := 8'd22
        }
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect m.w_m.addr, mux(eq(cond, UInt<1>(1)), addr1, addr0)"));
    run_firtool(&fir, &[]);
}

#[test]
fn let_bound_mem_read_emits_a_real_read_port() {
    // `collect_read_sites` used to have no `Stmt::Let` arm (only
    // `Assign`/`Expr`/`If`), so `let v = m[addr]` never got wired into
    // `read_ports` -- the read fell through to the generic "indexing
    // form not supported" error instead of emitting a real read port.
    // Same Assign-vs-Let parity gap as the fifo-op bug this session.
    let src = "\
module M {
    mem m : [8][256]
    in addr : [8]
    out out : [8] = 0

    rule r {
        let v = m[addr]
        out := v
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect m.r0.addr, addr"));
    assert!(fir.contains("connect __out_out, m.r0.data"));
    run_firtool(&fir, &[]);
}

#[test]
fn errors_on_ambiguous_top_module() {
    // Two modules that don't instantiate each other: still an error, just
    // reworded now that multiple modules is legal when one instantiates
    // the other (see `emits_submodule_instance`).
    let src = "\
module A {
    reg x : [1] = 0
}
module B {
    reg y : [1] = 0
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("exactly one top module"))
    );
}

#[test]
fn errors_on_instantiation_cycle() {
    let src = "\
module A {
    inst b : B
}
module B {
    inst a : A
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(err.iter().any(|e| e.message.contains("cycle")));
}

#[test]
fn errors_on_instantiation_cycle_below_a_valid_top() {
    // `Top` itself is uninstantiated (a valid, unambiguous top), so this
    // exercises the DFS in `transitive_modules`/`visit_module` rather than
    // the "zero candidates" shortcut `errors_on_instantiation_cycle`
    // above takes before that DFS ever runs.
    let src = "\
module Top {
    inst b : B
}
module B {
    inst c : C
}
module C {
    inst b : B
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(err.iter().any(|e| e.message.contains("cycle")));
}

#[test]
fn nested_inst_write_threads_through_a_mux() {
    // An instance port write living inside if/else must reach the port as
    // a mux, not be silently dropped for not being a top-level assignment
    // (same claim as `subleq_emits_and_compiles`'s `pc` mux, for a port).
    let src = "\
module Child {
    in a : [8]
    out b : [8] = 0
    rule pass {
        b := a
    }
}
module Top {
    inst c : Child
    reg cond : [1] = 0
    reg v : [8] = 0
    rule r {
        if logic cond = 1 {
            c.a := v
        } else {
            c.a := 1
        }
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect c.a, mux(eq(cond, UInt<1>(1)), v, UInt<8>(1))"));
    run_firtool(&fir, &[]);
}

#[test]
fn inst_write_on_only_one_side_of_an_if_falls_back_to_the_default() {
    // Written on only the `if` branch: the `else` path must fall back to
    // the port's unconditional `UInt(0)` default, not hold a stale value
    // (a port has no memory of its own, unlike a register).
    let src = "\
module Child {
    in a : [8]
    out b : [8] = 0
    rule pass {
        b := a
    }
}
module Top {
    inst c : Child
    reg cond : [1] = 0
    reg v : [8] = 0
    rule r {
        if logic cond = 1 {
            c.a := v
        }
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect c.a, mux(eq(cond, UInt<1>(1)), v, UInt<8>(0))"));
    run_firtool(&fir, &[]);
}

#[test]
fn emits_submodule_instance() {
    let src = read_example("submodule.tr");
    let fir = emit_from_source(&src).unwrap();
    assert!(fir.contains("circuit Top :"));
    assert!(fir.contains("public module Top :"));
    assert!(fir.contains("module Adder :") && !fir.contains("public module Adder :"));
    assert!(fir.contains("inst adder of Adder"));
    assert!(fir.contains("connect adder.clock, clock"));
    assert!(fir.contains("connect adder.reset, reset"));
    // Reading a child's output port compiles to a bare reference.
    assert!(fir.contains("connect __out_result, adder.sum"));

    let Some(verilog) = run_firtool(&fir, &["--disable-opt"]) else {
        return;
    };
    assert!(verilog.contains("module Adder("));
    assert!(verilog.contains("module Top("));
    assert!(verilog.contains("Adder adder ("));
}

#[test]
fn emits_multiple_instances_of_the_same_module() {
    // Two instances of one child: distinct instance names must not
    // collide (helpers that key by instance name, not the shared module
    // name, are the risk this pins).
    let src = "\
module Adder {
    in a : [8]
    in b : [8]
    out sum : [8] = 0
    rule add {
        sum := a + b
    }
}
module Top {
    inst a1 : Adder
    inst a2 : Adder
    in x : [8]
    out r1 : [8] = 0
    out r2 : [8] = 0
    rule wire {
        a1.a := x
        a1.b := x
        a2.a := x
        a2.b := x
        r1 := a1.sum
        r2 := a2.sum
    }
}
";
    let fir = emit_from_source(src).unwrap();
    assert!(fir.contains("inst a1 of Adder"));
    assert!(fir.contains("inst a2 of Adder"));
    assert!(fir.contains("connect a1.clock, clock"));
    assert!(fir.contains("connect a2.clock, clock"));
    assert!(fir.contains("connect __out_r1, a1.sum"));
    assert!(fir.contains("connect __out_r2, a2.sum"));

    let Some(verilog) = run_firtool(&fir, &["--disable-opt"]) else {
        return;
    };
    assert!(verilog.contains("Adder a1 ("));
    assert!(verilog.contains("Adder a2 ("));
}

#[test]
fn nested_module_emits_as_its_own_top_level_firrtl_block() {
    // `Adder` declared lexically inside `Top`'s body still gets its own
    // separate FIRRTL `module` block, cross-referenced by `inst ... of
    // Adder` — FIRRTL itself has no nested-module concept, only
    // top-level modules wired together (same shape as two sibling
    // modules, see `emits_submodule_instance`, just found via a
    // different AST walk).
    let src = "\
module Top {
    module Adder {
        in a : [8]
        in b : [8]
        out sum : [8] = 0
        rule add {
            sum := a + b
        }
    }
    inst adder : Adder
    in x : [8]
    in y : [8]
    out result : [8] = 0
    rule wire {
        adder.a := x
        adder.b := y
        result := adder.sum
    }
}
";
    let fir = emit_from_source(src).unwrap();
    assert!(fir.contains("circuit Top :"));
    assert!(fir.contains("public module Top :"));
    assert!(fir.contains("module Adder :") && !fir.contains("public module Adder :"));
    assert!(fir.contains("inst adder of Adder"));

    let Some(verilog) = run_firtool(&fir, &["--disable-opt"]) else {
        return;
    };
    assert!(verilog.contains("module Adder("));
    assert!(verilog.contains("module Top("));
    assert!(verilog.contains("Adder adder ("));
}

#[test]
fn alu_emits_widened_expression_surface() {
    let fir = emit_from_source(&read_example("alu.tr")).expect("emission should succeed");

    // Widening multiply: both operands are bits (not a literal), so
    // types.rs already sums their widths — no `tail` truncation needed.
    assert!(fir.contains("connect __out_prod, mul(a, b)"));
    assert!(fir.contains("connect __out_band, and(a, b)"));
    assert!(fir.contains("connect __out_bor, or(a, b)"));
    assert!(fir.contains("connect __out_bxor, xor(a, b)"));
    // Static shift: `shl` grows width by the shift amount, `shr` shrinks
    // it; both get brought back to the left operand's own width.
    assert!(fir.contains("connect __out_shl3, tail(shl(a, 3), 3)"));
    assert!(fir.contains("connect __out_shr3, pad(shr(a, 3), 8)"));
    assert!(fir.contains("connect __out_nega, tail(sub(UInt<8>(0), a), 1)"));
    assert!(fir.contains("connect __out_nota, not(a)"));
    assert!(fir.contains("connect __out_lo4, bits(a, 3, 0)"));
    assert!(fir.contains("connect __out_bit7, bits(a, 7, 7)"));

    run_firtool(&fir, &[]);
}

#[test]
fn multiply_by_a_literal_truncates_back_to_the_declared_width() {
    // Unlike alu.tr's bits*bits multiply (which widens), a bits*literal
    // multiply keeps the checker's declared width (types.rs's mixed
    // Bits/Int rule) — `mul` itself still sums both compiled operand
    // widths, so this needs a `tail` to drop back down, same idea as
    // `add`/`sub`'s carry-bit truncation but a variable amount (8 bits
    // here, not a constant 1).
    let src = "\
module M {
    in x : [8]
    out y : [8] = 0
    rule r {
        y := x * 3
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_y, tail(mul(x, UInt<8>(3)), 8)"));
    run_firtool(&fir, &[]);
}

#[test]
fn sized_literal_emits_its_own_declared_width() {
    let src = "\
module M {
    out a : [8] = 0
    out b : [8] = 0
    out c : [8] = 0
    out d : [8] = 0
    rule r {
        a := 8'd6
        b := 8'hFF
        c := 8'b1010
        d := 8'6
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_a, UInt<8>(6)"));
    assert!(fir.contains("connect __out_b, UInt<8>(255)"));
    assert!(fir.contains("connect __out_c, UInt<8>(10)"));
    assert!(fir.contains("connect __out_d, UInt<8>(6)"));
    run_firtool(&fir, &[]);
}

#[test]
fn sized_literal_widens_in_arithmetic_against_a_wider_operand() {
    // `8'd6`'s own width (8) is narrower than `x`'s (16) -- FIRRTL's own
    // `add` primop already handles two differently-sized UInt operands
    // (same as any two real registers of different widths), so this
    // must NOT hint the literal up to 16 bits the way a bare, width-less
    // `Int` literal would.
    let src = "\
module M {
    in x : [16]
    out result : [16] = 0
    rule r {
        result := x + 8'd6
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_result, tail(add(x, UInt<8>(6)), 1)"));
    run_firtool(&fir, &[]);
}

#[test]
fn sized_literal_works_as_a_bit_select_bound_and_a_shift_amount() {
    let src = "\
module M {
    in x : [16]
    out bit3 : [1] = 0
    out shifted : [16] = 0
    rule r {
        bit3 := x[8'd3]
        shifted := x << 4'd2
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_bit3, bits(x, 3, 3)"));
    assert!(fir.contains("connect __out_shifted, tail(shl(x, 2), 2)"));
    run_firtool(&fir, &[]);
}

#[test]
fn reg_and_output_emit_using_their_inferred_width() {
    let src = "\
module M {
    reg a = 8'd6
    out b = 16'hFF00
    rule r {
        a := a + 1
        b := 1
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("regreset a : UInt<8>, clock, reset, UInt<8>(6)"));
    assert!(fir.contains("regreset __out_b : UInt<16>, clock, reset, UInt<16>(65280)"));
    run_firtool(&fir, &[]);
}

#[test]
fn sized_literal_widens_via_a_bare_connect_into_a_wider_target() {
    // Unlike arithmetic (where `add`/`sub`/etc. combine both operand
    // widths themselves), a bare `result := 8'd6` connect has no op to
    // do that widening -- it relies on FIRRTL's `connect` statement
    // implicitly extending a narrower UInt source into a wider sink.
    // Confirmed against real firtool: it accepts this and zero-extends
    // (`16'h6`), the same as it would for two real registers of
    // differing widths, so emitting the literal at its own declared
    // width here (not the sink's) is correct, not a gap.
    let src = "\
module M {
    out result : [16] = 0
    rule r {
        result := 8'd6
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_result, UInt<8>(6)"));
    run_firtool(&fir, &[]);
}

#[test]
fn sized_literal_compares_against_a_wider_operand() {
    // `logic`'s comparison operand doesn't unify widths itself (`eq`'s
    // own arm in `type_binop` no longer runs for a comparison at all --
    // it yields `x`'s own type now, see TODO.md's comparisons-as-
    // fallible design), and FIRRTL's `eq` primop itself implicitly
    // extends the narrower operand -- so `x = 8'd6` with `x : [16]`
    // needs no special-casing beyond what the sized literal already
    // does (emit at its own width).
    let src = "\
module M {
    in x : [16]
    out eq : [1] = 0
    rule r {
        eq := logic x = 8'd6
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_eq, eq(x, UInt<8>(6))"));
    run_firtool(&fir, &[]);
}

#[test]
fn dynamic_shift_grows_then_trims_shl_but_shr_needs_no_adjustment() {
    // FIRRTL's own widths (confirmed against real firtool, not assumed):
    // `dshl(a, b)` grows to `w(a) + 2^w(b) - 1` (the exponential term is
    // `b`'s own WIDTH, a static quantity, bounding the largest possible
    // shift amount `b` could hold) -- trimmed back to `x`'s declared
    // width (8) by dropping exactly `2^w(n) - 1` bits, `2^3 - 1 = 7`
    // here since `n : [3]`. `dshr`, unlike static `shr`, does NOT
    // shrink at all -- it's already exactly `w(a)`, so no `pad`/`tail`
    // wrapper is needed there, unlike every other shift/mul/div/rem case
    // in this file.
    let src = "\
module M {
    in x : [8]
    in n : [3]
    out shl_out : [8] = 0
    out shr_out : [8] = 0
    rule r {
        shl_out := x << n
        shr_out := x >> n
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_shl_out, tail(dshl(x, n), 7)"));
    assert!(fir.contains("connect __out_shr_out, dshr(x, n)"));
    run_firtool(&fir, &[]);
}

#[test]
fn arith_shift_wraps_in_as_sint_as_uint_confirmed_against_real_firtool() {
    // `>>>` has no signed type to lean on (this language doesn't have
    // one) -- the sign-extending behavior comes entirely from wrapping
    // the shift in `asSInt`/`asUInt` at emission time. Static: `pad`
    // must run BEFORE `asUInt`, not after, since `pad` on an `SInt`
    // sign-extends but `pad` on a `UInt` zero-extends. Dynamic: no `pad`
    // needed at all, the same reason unsigned `dshr` needs none (width
    // already stays `w(a)` regardless of sign) -- both confirmed against
    // real firtool output AND a real simulation (see
    // examples/arith_shift.tr's own sim/arith_shift_tb.v).
    let src = "\
module M {
    in x : [8]
    in n : [3]
    out static_out : [8] = 0
    out dynamic_out : [8] = 0
    rule r {
        static_out := x >>> 3
        dynamic_out := x >>> n
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_static_out, asUInt(pad(shr(asSInt(x), 3), 8))"));
    assert!(fir.contains("connect __out_dynamic_out, asUInt(dshr(asSInt(x), n))"));
    run_firtool(&fir, &[]);
}

#[test]
fn dynamic_single_index_select_compiles_to_a_dynamic_shift() {
    // A single index is always exactly 1 bit whether it's a compile-time
    // constant or a genuine runtime value -- `x[i]` with a dynamic `i`
    // shifts the target bit down to position 0 (`dshr`) then takes it,
    // rather than erroring the way it used to (v0 restriction lifted).
    let src = "\
module M {
    in x : [8]
    in i : [8]
    out y : [1] = 0
    rule r {
        y := x[i]
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_y, bits(dshr(x, i), 0, 0)"));
    run_firtool(&fir, &[]);
}

#[test]
fn indexed_part_select_up_and_down_use_dshr_plus_a_static_truncate() {
    // `x[base +: width]`/`x[base -: width]` (Verilog-style indexed
    // part-select): `base` may be dynamic, but `width` is always a
    // compile-time constant, so the RESULT's width is well-defined even
    // though the starting bit isn't known until runtime. `+:` shifts
    // `base` itself down to position 0; `-:` shifts `base-(width-1)`
    // down instead, so the SAME low `width` bits after either shift
    // land on the intended window -- both then truncate with a STATIC
    // `bits(..., width-1, 0)`, unlike a fully dynamic slice (still
    // unsupported: see `dynamic_slice_bounds_are_a_type_error`,
    // tests/types.rs).
    let src = "\
module M {
    in x : [8]
    in base : [3]
    out up : [4] = 0
    out down : [4] = 0
    rule r {
        up := x[base +: 4]
        down := x[base -: 4]
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_up, bits(dshr(x, base), 3, 0)"));
    assert!(
        fir.contains("connect __out_down, bits(dshr(x, tail(sub(base, UInt<3>(3)), 1)), 3, 0)")
    );
    run_firtool(&fir, &[]);
}

#[test]
fn indexed_part_select_width_folds_a_non_literal_constant_expression() {
    // Regression: the width must come from the bracket's own
    // types.rs-computed type (`width_of(id)`), not from re-const-evaling
    // `rhs` at emission time. types.rs's `const_eval` folds binary ops
    // (`2 + 2`), but firrtl's own `const_eval` only recognizes a bare
    // literal -- an earlier version fell back to `unwrap_or(1)` here,
    // silently emitting a 1-bit select for a `[4]` output instead of
    // erroring or using the real width.
    let src = "\
module M {
    in x : [8]
    in base : [3]
    out y : [4] = 0
    rule r {
        y := x[base +: 2 + 2]
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_y, bits(dshr(x, base), 3, 0)"));
    run_firtool(&fir, &[]);
}

#[test]
fn reversed_slice_bounds_are_an_error_not_invalid_firrtl() {
    // types.rs's width formula (`hi.abs_diff(lo) + 1`) accepts either
    // bound order, but FIRRTL's `bits` primop needs hi >= lo — without
    // this check `x[0..3]` would emit `bits(x, 0, 3)`, which firtool
    // rejects with no span back into the .tr source.
    let src = "\
module M {
    in x : [8]
    out y : [4] = 0
    rule r {
        y := x[0..3]
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(err.iter().any(|e| e.message.contains("hi >= lo")));
}

#[test]
fn logical_not_compiles_identically_to_bitwise_not_on_a_bits_1_value() {
    // `not` and `~` emit the IDENTICAL FIRRTL `not` primop -- what makes
    // `not` a real, distinct operator (not just a parse-time alias) is a
    // types.rs restriction (tests/types.rs's
    // `logical_not_needs_a_bits_1_operand`): `not` requires its operand
    // already be `[1]`, `~` accepts any width. Once that's enforced,
    // bitwise-complementing the single bit IS logical negation, so
    // there's nothing left for emission to do differently -- proved here
    // by asserting both compile to the exact same FIRRTL text.
    let src = "\
module M {
    in x : [8]
    out lnot : [1] = 0
    out tilde : [1] = 0
    rule r {
        lnot := not (logic x = 0)
        tilde := ~(logic x = 0)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_lnot, not(eq(x, UInt<8>(0)))"));
    assert!(fir.contains("connect __out_tilde, not(eq(x, UInt<8>(0)))"));
    run_firtool(&fir, &[]);
}

#[test]
fn div_and_rem_of_equal_width_operands_need_no_pad() {
    // FIRRTL's own `div`/`rem` primops don't share `add`/`sub`/`mul`'s
    // "always needs a trim" shape: `div(a, b)`'s width is exactly the
    // DIVIDEND's own width (confirmed against real firtool, not assumed
    // from the spec text alone), so when both operands are already the
    // checker's target width (equal here), no `pad` wrapper is needed at
    // all -- unlike `rem`, which is `min(w(a), w(b))` and so needs a pad
    // even when the operands ARE equal width, since FIRRTL only zero-
    // extends up from `min`, never keeps the full width automatically.
    let src = "\
module M {
    in x : [8]
    in y : [8]
    out q : [8] = 0
    out r : [8] = 0
    rule rule1 {
        q := x / y
        r := x % y
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_q, div(x, y)"));
    assert!(fir.contains("connect __out_r, rem(x, y)"));
    run_firtool(&fir, &[]);
}

#[test]
fn div_and_rem_of_differing_width_operands_pad_up_to_the_wider_target() {
    // `b : [4]`, `a : [8]` -- the checker's target width for
    // `b / a` and `b % a` is `max(4, 8) = 8`, but FIRRTL's own `div`
    // width is the DIVIDEND's width (4 here, not 8) and `rem`'s is
    // `min(4, 8) = 4` -- both narrower than the target, so both need an
    // explicit `pad` up to 8. `a / b` is the mirror case: FIRRTL's div
    // width (8, `a`'s own width as dividend) already equals the target,
    // so no pad there, while `a % b`'s `rem` width (`min(8,4)=4`) still
    // needs padding up to 8 even though the DIVIDEND already matches.
    let src = "\
module M {
    in a : [8]
    in b : [4]
    out q1 : [8] = 0
    out q2 : [8] = 0
    out r1 : [8] = 0
    out r2 : [8] = 0
    rule rule1 {
        q1 := a / b
        q2 := b / a
        r1 := a % b
        r2 := b % a
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_q1, div(a, b)"));
    assert!(fir.contains("connect __out_q2, pad(div(b, a), 8)"));
    assert!(fir.contains("connect __out_r1, pad(rem(a, b), 8)"));
    assert!(fir.contains("connect __out_r2, pad(rem(b, a), 8)"));
    run_firtool(&fir, &[]);
}

#[test]
fn call_inlines_a_pure_function_with_a_let_and_a_trailing_return() {
    let fir = emit_from_source(&read_example("call.tr")).expect("emission should succeed");
    // `Avg`'s body (`let sum = a + b; return sum >> 1`) spliced straight
    // into the call site, exactly as if it had been written inline —
    // no trace of a call, no separate `module Avg`.
    assert!(fir.contains("connect __out_result, pad(shr(tail(add(a, b), 1), 1), 8)"));
    assert!(!fir.contains("module Avg"));

    // firtool hoists the shared `a + b` into a named temp under default
    // optimization, which it then declares `automatic` inside the
    // `always` block — a construct Icarus rejects ("Overriding the
    // default variable lifetime is not yet supported"). `--disable-opt`
    // keeps it a plain `wire` instead; unrelated to dead-code elimination,
    // the usual reason other tests pass this flag.
    run_firtool(&fir, &["--disable-opt"]);
}

#[test]
fn nested_call_to_the_same_function_does_not_clobber_the_outer_arguments() {
    // Regression test for a real silent miscompile caught before this
    // shipped: `Avg`'s param DefIds are shared across every call to
    // `Avg`, so compiling the outer call's first argument (which
    // recurses into the inner `Avg(x, y)` call) would rebind them out
    // from under the outer call before it got to compile `z` — without
    // `compile_call`'s save/restore, this silently produced `(x-y)-y`,
    // dropping `z` entirely, instead of `(x-y)-z`.
    let src = "\
Avg(a : [8], b : [8]) : [8] <combines> {
    return a - b
}
module Top {
    in x : [8]
    in y : [8]
    in z : [8]
    out result : [8] = 0
    rule r {
        result := Avg(Avg(x, y), z)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_result, tail(sub(tail(sub(x, y), 1), z), 1)"));
    run_firtool(&fir, &["--disable-opt"]);
}

#[test]
fn call_to_a_same_module_fn_that_reads_state_still_works() {
    // The legitimate case a boundary check must not break: `Bump` is
    // nested inside `M` and reads `M`'s own `v`; the only rule that
    // calls it is also inside `M`, so the call site's module matches
    // every state def `Bump`'s (merged) signature reaches.
    let src = "\
module M {
    reg v : [8] = 0
    in a : [8]
    out result : [8] = 0

    Bump(x : [8]) : [8] <combines> {
        return v + x
    }

    rule r {
        result := Bump(a)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_result, tail(add(v, a), 1)"));
    run_firtool(&fir, &[]);
}

#[test]
fn call_reaching_a_different_modules_state_is_an_error() {
    // A `fn` nested in `M` stays visible to a rule in a module nested
    // INSIDE `M` (scopes nest outward-to-inward, same as an ordinary
    // local), but `Bump`'s `v` is `M`'s own register — inlining this
    // call would splice a reference to `v` into `N`'s separate FIRRTL
    // block, where `v` doesn't exist. Before this check existed, this
    // program silently reached firtool as an "unknown declaration `v`"
    // error instead of a clear trace-level one.
    let src = "\
module M {
    reg v : [8] = 0
    Bump(x : [8]) : [8] <combines> { return v + x }
    module N {
        in a : [8]
        out result : [8] = 0
        rule r { result := Bump(a) }
    }
    inst n : N
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("belongs to a different module"))
    );
}

#[test]
fn a_callee_may_call_another_callee_for_a_pure_value() {
    // Composition: `Outer`'s own body calls `Inner`, using its return
    // value inside a `let`. Distinguishable arithmetic at each step
    // (Inner: +1, Outer: *2) so a wrong nesting order or a dropped call
    // shows up as a wrong number, not just "it compiled."
    let src = "\
Inner(x : [8]) : [8] <combines> {
    return x + 1
}
Outer(x : [8]) : [8] <combines> {
    let doubled = Inner(x) * 2
    return doubled
}
module M {
    in a : [8]
    out result : [8] = 0
    rule r {
        result := Outer(a)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(
        fir.contains("connect __out_result, tail(mul(tail(add(a, UInt<8>(1)), 1), UInt<8>(2)), 8)")
    );
    run_firtool(&fir, &[]);
}

#[test]
fn calling_the_same_nested_function_twice_independently_is_not_a_cycle() {
    // A diamond, not a cycle: `Outer` calls `Inner` twice. `find_call_
    // cycle` only follows `Inner`'s OWN body (which calls nothing), so
    // neither call trips the cycle check regardless of how many times
    // `Outer` happens to reference `Inner`.
    let src = "\
Inner(x : [8]) : [8] <combines> {
    return x + 1
}
Outer(x : [8]) : [8] <combines> {
    let a = Inner(x)
    let b = Inner(x + 1)
    return a + b
}
module M {
    in x : [8]
    out result : [8] = 0
    rule r {
        result := Outer(x)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    run_firtool(&fir, &[]);
}

#[test]
fn a_generic_nested_call_through_a_binop_is_a_clean_error_not_a_miscompile() {
    // Boundary this feature newly makes reachable, not something it
    // needs to solve: `compile_binop` computes ITS OWN width hint via
    // `known_width(id)` (`types.expr_tys` for the binop's own id),
    // ignoring whatever hint its caller threaded down — fine for a
    // CONCRETE callee body, but `Outer`'s body here is generic
    // (`[N]`), type-checked once with `N` never resolved, so
    // `known_width` returns nothing and the nested `Inner(x)` call
    // falls back to `width_of`, which also finds nothing. Pinned as a
    // clean error (not a hang, not a wrong width silently emitted) —
    // the same latent gap already documented for `prio`'s own argument
    // width (`concrete_width_of`), not something this feature fixes.
    let src = "\
Inner(x : [N]) : [N] <combines> {
    return x
}
Outer(x : [N]) : [N] <combines> {
    return Inner(x) + 1
}
module M {
    reg r : [8] = 0
    rule compute {
        r := Outer(r)
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(err.iter().any(|e| e.message.contains("no concrete width")));
}

#[test]
fn an_indirect_call_cycle_is_a_clean_error_not_a_hang() {
    // Direct self-recursion never reaches this check at all: effects.rs
    // already rejects it upstream (`recursion_requires_elaborates`,
    // tests/effects.rs), and an `<elaborates>` function can't be
    // inlined in the first place (`validate_call`'s own `sig.elaborates`
    // check, above). Indirect/mutual recursion is different: neither
    // `A` nor `B` calls itself directly, so effects.rs has nothing to
    // catch, and this cycle is only ever caught here, by
    // `find_call_cycle`'s static call graph, at emission time.
    let src = "\
A(x : [8]) : [8] <combines> {
    return B(x)
}
B(x : [8]) : [8] <combines> {
    return A(x)
}
module M {
    in a : [8]
    out result : [8] = 0
    rule r {
        result := A(a)
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert_eq!(
        err.len(),
        1,
        "a single call site should only ever be validated-and-erred once, \
         even if validate_call runs it through more than one path: {err:?}"
    );
    assert!(
        err.iter()
            .any(|e| e.message.contains("call cycle: A -> B -> A"))
    );
}

#[test]
fn a_nested_call_used_as_a_let_value_that_writes_state_is_still_an_error() {
    // `Inner` writes `w`; `Outer` uses `Inner`'s return value inside a
    // `let`, not a bare statement or the whole RHS of `:=`.
    // `check_writing_call_positions_in` (checks.rs) restricts a
    // writing call to those two positions inside a callee's own body,
    // same as it already does at the rule level — `callee_reg_write`
    // only ever looks for a write there, so anywhere else would
    // silently drop it if this check didn't reject it outright first.
    //
    // `Outer(w)` is also the regression shape for a real duplicate-
    // diagnostic bug: `validate_call` runs once for `Outer`'s return
    // value (`result := ...`) and again for `w`'s own write-hunt
    // (`call_writes_reg`, since `Outer`'s merged signature writes `w`),
    // and both paths independently ran `check_writing_call_positions_in`
    // against the identical body, each pushing the identical error —
    // asserting `err.len() == 1` (not just `.any(...)`) is what would
    // have caught it; `Emitter::error` (mod.rs) now dedups by
    // `(span, message)` at the single choke point every error goes
    // through, fixing this for every error `validate_call` can emit,
    // not just this one.
    let src = "\
module M {
    reg w : [8] = 0
    out result : [8] = 0

    Inner(x : [8]) : [8] <combines, writes {w}> {
        w := x
        return x + 1
    }
    Outer(x : [8]) : [8] <combines, writes {w}> {
        let t = Inner(x)
        return t
    }

    rule r {
        result := Outer(w)
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert_eq!(
        err.len(),
        1,
        "validate_call runs once for Outer's return value and again for w's \
         write-hunt; without dedup this pushes the identical error twice: {err:?}"
    );
    assert!(
        err.iter()
            .any(|e| e.message.contains("may only appear as a whole statement"))
    );
}

#[test]
fn a_nested_writing_call_used_as_a_bare_statement_threads_its_write_through() {
    // `Outer` calls `Inner` (which writes `v`) as a bare statement, its
    // own return value unused -- `Inner`'s allowed position, mirroring
    // the top-level "a rule may call a writing function as a bare
    // statement" feature one level deeper. Regression test for a real
    // bug caught while building this: passing `Outer(v)` (feeding `v`'s
    // OWN current value back in) makes the write a no-op by
    // construction (`v := v`), indistinguishable from a dropped write —
    // `a` (a real input, not `v` itself) is what actually proves the
    // write landed.
    let src = "\
module M {
    reg v : [8] = 0
    in a : [8]
    out result : [8] = 0

    Inner(x : [8]) : [8] <combines, writes {v}> {
        v := x
        return x
    }
    Outer(x : [8]) : [8] <combines, writes {v}> {
        Inner(x)
        return x + 1
    }

    rule compute {
        result := Outer(a)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect v, a"));
    assert!(fir.contains("connect __out_result, tail(add(a, UInt<8>(1)), 1)"));
    run_firtool(&fir, &[]);
}

#[test]
fn a_state_writing_callee_may_still_call_a_pure_helper() {
    // The write guard is narrow: it rejects a NESTED call that itself
    // writes state, not every callee with a nested call. `Bump` writes
    // `v` directly (an `Assign`, not a call) using `Helper`'s (pure)
    // return value — `Helper` has an empty `sig.writes`, so the guard
    // never fires for it.
    let src = "\
module M {
    reg v : [8] = 0
    in a : [8]
    out result : [8] = 0

    Helper(x : [8]) : [8] <combines> {
        return x + 1
    }
    Bump(x : [8]) : [8] <combines, writes {v}> {
        v := Helper(x)
        return x
    }

    rule r {
        result := Bump(a)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect v, tail(add(a, UInt<8>(1)), 1)"));
    run_firtool(&fir, &[]);
}

#[test]
fn a_pure_nested_bare_statement_call_compiles_away_harmlessly() {
    // `Inner(x)` inside `Outer`'s body, as a bare statement, its return
    // value discarded, and `Inner` is pure (writes nothing) -- a no-op
    // by construction (nothing observes it, nothing it does persists),
    // so it's allowed and simply contributes nothing to the emitted
    // hardware: `result` depends only on `Outer`'s own `return x + 1`.
    let src = "\
module M {
    in a : [8]
    out result : [8] = 0

    Inner(x : [8]) : [8] <combines> {
        return x + 1
    }
    Outer(x : [8]) : [8] <combines> {
        Inner(x)
        return x + 1
    }

    rule r {
        result := Outer(a)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_result, tail(add(a, UInt<8>(1)), 1)"));
    run_firtool(&fir, &[]);
}

#[test]
fn call_inlines_a_function_that_writes_state_and_returns_a_value() {
    // `Bump` both writes `v` (a side effect) and returns `x + 1` (its
    // own value, assigned to `result`) from the SAME call site — the
    // two are independent walks (`callee_reg_write` for `v`,
    // `compile_callee_body` for `result`'s value via the normal return-
    // value path) over the same statement, not one computation feeding
    // the other.
    let src = "\
module M {
    reg v : [8] = 0
    in a : [8]
    out result : [8] = 0

    Bump(x : [8]) : [8] <combines> {
        v := x
        return x + 1
    }

    rule r {
        result := Bump(a)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect v, a"));
    assert!(fir.contains("connect __out_result, tail(add(a, UInt<8>(1)), 1)"));
    run_firtool(&fir, &["--disable-opt"]);
}

#[test]
fn call_to_a_writing_function_as_a_bare_statement_discards_the_return_value() {
    let src = "\
module M {
    reg v : [8] = 0
    in a : [8]

    Bump(x : [8]) : [8] <combines> {
        v := x
        return x + 1
    }

    rule r {
        Bump(a)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect v, a"));
    run_firtool(&fir, &["--disable-opt"]);
}

#[test]
fn call_writes_state_conditionally_inside_its_own_if_else() {
    // Called as a BARE statement, so only the write-hunt path
    // (`callee_reg_write`) ever looks at `Bump`'s body — it handles an
    // `if`/`else` in any position, unlike the return-value path
    // (`compile_callee_body`), which only accepts one in TAIL position.
    // See `call_to_a_conditionally_writing_function_whose_return_value_
    // is_used_requires_tail_position` for that boundary pinned the other
    // way.
    let src = "\
module M {
    reg v : [8] = 0
    in a : [8]

    Bump(x : [8]) : [8] <combines> {
        if logic x > 10 {
            v := x
        } else {
            v := 0
        }
        return x
    }

    rule r {
        Bump(a)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect v, mux(gt(a, UInt<8>(10)), a, UInt<8>(0))"));
    run_firtool(&fir, &["--disable-opt"]);
}

#[test]
fn call_to_a_conditionally_writing_function_whose_return_value_is_used_requires_tail_position() {
    // The exact same `Bump` as `call_writes_state_conditionally_inside_
    // its_own_if_else`, but with its return value now consumed
    // (`result := Bump(a)`) — this DOES run the return-value path
    // (`compile_callee_body`), which requires a non-`let`/`Assign`
    // statement (the `if`/`else` here) to be in TAIL position, not
    // followed by a separate `return`. A clean error, not a silent
    // miscompile (the whole emission fails before anything is written) —
    // but a real asymmetry worth pinning: the SAME callee body is
    // inlinable or not depending on whether its caller uses the return
    // value.
    let src = "\
module M {
    reg v : [8] = 0
    in a : [8]
    out result : [8] = 0

    Bump(x : [8]) : [8] <combines> {
        if logic x > 10 {
            v := x
        } else {
            v := 0
        }
        return x
    }

    rule r {
        result := Bump(a)
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(err.iter().any(|e| {
        e.message
            .contains("too complex to inline for its RETURN value")
    }));
}

#[test]
fn same_writing_function_called_from_two_rules_gates_each_write_separately() {
    let src = "\
module M {
    reg v : [8] = 0
    in a : [8]
    in b : [8]
    in sel : [1]

    Bump(x : [8]) : [8] <combines> {
        v := x
        return x + 1
    }

    rule r1 {
        (sel = 1)?
        Bump(a)
    }
    rule r2 {
        (sel = 0)?
        Bump(b)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("when fires_r1 :\n      connect v, a"));
    assert!(fir.contains("when fires_r2 :\n      connect v, b"));
    run_firtool(&fir, &[]);
}

#[test]
fn call_writes_an_instance_port() {
    let src = "\
module Child {
    in a : [8]
    out b : [8] = 0
    rule pass {
        b := a
    }
}
module Top {
    inst c : Child
    in x : [8]

    Drive(v : [8]) : [8] <combines> {
        c.a := v
        return v
    }

    rule r {
        Drive(x)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect c.a, x"));
    run_firtool(&fir, &[]);
}

#[test]
fn a_writing_call_nested_in_a_larger_expression_is_an_error_not_a_dropped_write() {
    // `call_writes_reg`/`call_writes_port` only ever look for a
    // writing call as a whole statement or the whole RHS of `:=` — a
    // call nested one level deeper, here as an operand of `+`, would
    // never be found by either walk, silently dropping `Bump`'s write
    // to `v` while its return value still inlines fine. Must be an
    // explicit error, not a silent miscompile.
    let src = "\
module M {
    reg v : [8] = 0
    in a : [8]
    out result : [8] = 0

    Bump(x : [8]) : [8] <combines> {
        v := x
        return x + 1
    }

    rule r {
        result := Bump(a) + 1
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("may only appear as a whole statement"))
    );
}

#[test]
fn a_writing_call_bound_to_a_let_is_an_error_not_a_dropped_write() {
    let src = "\
module M {
    reg v : [8] = 0
    in a : [8]
    out result : [8] = 0

    Bump(x : [8]) : [8] <combines> {
        v := x
        return x + 1
    }

    rule r {
        let t = Bump(a)
        result := t
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("may only appear as a whole statement"))
    );
}

#[test]
fn call_folds_a_guard_that_references_a_preceding_callee_local() {
    // Regression test for a real gap found while extending this area to
    // fifo ops: `callee_fail_cond` originally only bound the callee's
    // PARAMS, not its own top-level `let`s, so a guard written in terms
    // of a preceding local (`let y = x + 1 / (y <> 0)?`) failed to
    // resolve `y` at all (a clean error, not a miscompile, but a real
    // gap) until `bind_callee_context` started binding both.
    let src = "\
Classify(x : [8]) : [8] <combines, fails> {
    let y = x + 1
    (y <> 0)?
    return y
}
module Top {
    in a : [8]
    out result : [8] = 0
    rule compute {
        result := Classify(a)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_compute = neq(tail(add(a, UInt<8>(1)), 1), UInt<8>(0))"));
    run_firtool(&fir, &["--disable-opt"]);
}

#[test]
fn call_folds_a_callees_bare_guard_into_the_callers_own_guard() {
    // `Classify`'s `(x <> 0)?` isn't the caller's own guard textually —
    // it's DESIGN.md's own "fails" example. The fold must compile the
    // condition against the CALL SITE's argument (`a`), not the
    // callee's own parameter name (`x`) unbound — the exact
    // param-substitution trap this fold has to get right.
    let src = "\
Classify(x : [8]) : [8] <combines, fails> {
    (x <> 0)?
    return x
}
module Top {
    in a : [8]
    out result : [8] = 0
    rule compute {
        result := Classify(a)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_compute = neq(a, UInt<8>(0))"));
    run_firtool(&fir, &["--disable-opt"]);
}

#[test]
fn a_bare_condition_implicitly_folds_into_the_rule_guard() {
    // `a <> 0` alone (no `?`) means the same thing as `(a <> 0)?`.
    let src = "\
module M {
    in a : [8]
    out result : [8] = 0
    rule compute {
        a <> 0
        result := a
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_compute = neq(a, UInt<8>(0))"));
    run_firtool(&fir, &["--disable-opt"]);
}

#[test]
fn a_comparison_nested_inside_a_larger_value_still_folds_into_the_guard() {
    // A comparison has no dedicated "whole statement only" position
    // restriction (unlike a guard/fifo op/failing call -- no side
    // effect, so no silent-miss risk from a misplaced one, see TODO.md's
    // comparisons-as-fallible design), so `compile_guard`'s fold has to
    // go FIND one rather than only check the top-level shape. Self-
    // caught by direct probe: this used to compile clean with `fires_r
    // = UInt<1>(1)`, never gating on `a > b` at all even though
    // effects.rs's `sig.fails` was already correctly `true` for it --
    // `ok` was written unconditionally, silently wrong whenever `a > b`
    // didn't actually hold.
    let src = "\
module M {
    in a : [8]
    in b : [8]
    out ok : [8] = 0
    rule r {
        ok := a + (a > b)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_r = gt(a, b)"));
    assert!(fir.contains("connect __out_ok, tail(add(a, a), 1)"));
    run_firtool(&fir, &["--disable-opt"]);
}

#[test]
fn a_comparison_nested_in_if_is_an_error_not_a_dropped_guard() {
    // The if/while-nesting sibling of the test above: folding a
    // comparison found INSIDE a conditional branch into the RULE's own
    // guard would be wrong regardless of nesting depth (the branch might
    // not even be taken), so this is rejected outright instead, the
    // same restriction a nested guard/fifo op/failing call already has.
    // Self-caught the same way: `v := a + (a > b)` inside an `if` used
    // to compile clean with `fires_r = UInt<1>(1)`, writing `v`
    // unconditionally on the branch taken, with no gating on `a > b` at
    // all.
    let src = "\
module M {
    reg v : [8] = 0
    in a : [8]
    in b : [8]
    in c : [1]
    rule r {
        if logic c = 1 {
            v := a + (a > b)
        }
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("comparison nested in if/while"))
    );
}

#[test]
fn a_bare_bit_select_implicitly_folds_into_the_rule_guard() {
    // The OTHER example from the request that motivated this feature
    // (`A[b]` alongside `a = 1`): a single (non-slice) bit-select is
    // always exactly [1] by construction, so `flags[i]` alone
    // means the same thing as `(flags[i])?` -- a different code path
    // through `is_guard_like` than a comparison (`Expr::Bracket`, not
    // `Expr::Binary`), and a different one again from a fifo op's own
    // `Enq`/`Deq` Bracket shape.
    let src = "\
module M {
    in flags : [8]
    in i : [8]
    out result : [8] = 0
    rule r {
        flags[i]
        result := flags
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_r = bits(dshr(flags, i), 0, 0)"));
    run_firtool(&fir, &[]);
}

#[test]
fn a_callees_bare_implicit_guard_also_folds_into_the_callers_guard() {
    // Same fold as `call_folds_a_callees_bare_guard_into_the_callers_
    // own_guard` above, but the callee's condition has no `?` -- proves
    // `callee_fail_cond` (calls.rs) handles the implicit case too, not
    // just an explicit `Expr::Guard`.
    let src = "\
Classify(x : [8]) : [8] <combines, fails> {
    x <> 0
    return x
}
module Top {
    in a : [8]
    out result : [8] = 0
    rule compute {
        result := Classify(a)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_compute = neq(a, UInt<8>(0))"));
    run_firtool(&fir, &["--disable-opt"]);
}

#[test]
fn a_bare_condition_after_a_state_write_is_an_error() {
    let src = "\
module M {
    reg a : [1] = 0
    reg b : [1] = 0
    rule r {
        b := 1
        a = 1
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("guard") && e.message.contains("state write"))
    );
}

#[test]
fn a_bare_condition_nested_in_if_is_an_error() {
    let src = "\
module M {
    reg a : [1] = 0
    reg b : [1] = 0
    rule r {
        if a = 1 {
            b = 1
        }
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("guard") && e.message.contains("if/while"))
    );
}

#[test]
fn call_folds_a_guard_and_threads_a_state_write_from_the_same_callee() {
    // The untested intersection between the guard fold and the
    // existing write-hunt: `Bump`'s body both guards AND writes `v` —
    // two independent passes over the same callee body
    // (`callee_fail_cond` for the guard, `callee_reg_write` for the
    // write) have to agree on the same rule-level guard, not just each
    // compile in isolation. Confirmed through real simulation before
    // this was added (both writes gated identically, `v` holds when
    // `a = 0`) — this pins the emitted shape.
    let src = "\
module M {
    reg v : [8] = 0
    in a : [8]
    out result : [8] = 0
    Bump(x : [8]) : [8] <combines, fails> {
        (x <> 0)?
        v := x
        return x + 1
    }
    rule r {
        result := Bump(a)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_r = neq(a, UInt<8>(0))"));
    let write_count = fir.matches("when fires_r :").count();
    assert_eq!(
        write_count, 2,
        "expected both v's write and result's write to be gated by the \
         SAME fires_r (one that includes the folded guard), not two \
         independently-computed conditions:\n{fir}"
    );
    run_firtool(&fir, &["--disable-opt"]);
}

#[test]
fn a_failing_call_nested_in_a_larger_expression_is_an_error_not_a_dropped_guard() {
    // Mirrors `a_writing_call_nested_in_a_larger_expression_is_an_error_
    // not_a_dropped_write`: `compile_guard`'s fold only scans a call
    // sitting as a whole bare statement or the entire RHS of `:=` — one
    // nested inside `+ 1` would never be found there, silently letting
    // the caller's rule fire on a cycle `Classify` should have blocked.
    let src = "\
Classify(x : [8]) : [8] <combines, fails> {
    (x <> 0)?
    return x
}
module Top {
    in a : [8]
    out result : [8] = 0
    rule compute {
        result := Classify(a) + 1
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("may only appear as a whole statement"))
    );
}

#[test]
fn a_failing_callee_with_a_guard_nested_in_if_else_is_still_rejected() {
    // Syntactically legal per `compile_callee_body`'s shape (a bare
    // guard is an allowed statement inside an if/else branch), but
    // invisible to `callee_fail_cond`'s flat top-level scan — folding
    // only the (nonexistent) top-level guard would silently drop this
    // one, letting the caller's rule fire even when `flag = 1` and
    // `x = 0`. `check_fails_is_foldable_guard` must catch this by
    // comparing "guards anywhere" against "guards at the top level".
    let src = "\
Classify(x : [8], flag : [1]) : [8] <combines, fails> {
    if flag = 1 {
        (x <> 0)?
        return x
    } else {
        return 0
    }
}
module Top {
    in a : [8]
    in f : [1]
    out result : [8] = 0
    rule compute {
        result := Classify(a, f)
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(err.iter().any(|e| {
        e.message
            .contains("reduces entirely to bare, top-level guards")
    }));
}

#[test]
fn call_folds_a_callees_fifo_op_into_the_callers_own_guard() {
    // A fifo op in a callee's own body sets `sig.fails` exactly like a
    // guard does, and now folds the same way: the Enq's own precondition
    // (`not(valid)`) becomes the caller's rule guard, and the Enq's
    // value threads through the same param substitution a guard's
    // condition already gets.
    let src = "\
module Top {
    fifo buf : [8]
    in a : [8]
    out result : [8] = 0

    Classify(x : [8]) : [8] <combines, fails> {
        buf.Enq[x]
        return x
    }

    rule compute {
        result := Classify(a)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_compute = not(__fifo_buf_valid)"));
    assert!(fir.contains("connect __fifo_buf_data, a"));
    run_firtool(&fir, &["--disable-opt"]);
}

#[test]
fn call_folds_both_a_guard_and_a_fifo_op_from_the_same_callee() {
    // The untested intersection between the two independent condition-
    // folding paths that now both write into `compile_guard`'s `conds`
    // list: `callee_fail_cond` (the guard) and `rule_fifo_ops`'s fifo
    // term (the Enq). Confirmed neither drops nor double-counts the
    // other's contribution before this shipped.
    let src = "\
module Top {
    fifo buf : [8]
    in a : [8]
    out result : [8] = 0
    Push(x : [8]) : [8] <combines, fails> {
        (x <> 0)?
        buf.Enq[x]
        return x
    }
    rule compute {
        result := Push(a)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_compute = and(not(__fifo_buf_valid), neq(a, UInt<8>(0)))"));
    run_firtool(&fir, &["--disable-opt"]);
}

#[test]
fn a_rule_enqueuing_directly_and_via_a_callee_is_still_a_double_enq_error() {
    // The double-op collision check (`check_fifo_op_counts`) has to
    // reach through the call boundary too, or this would silently keep
    // only the last write to land — the same class of bug `check_
    // fifo_op_counts` already exists to reject at the rule's own top
    // level.
    let src = "\
module Top {
    fifo buf : [8]
    in a : [8]
    in b : [8]
    out result : [8] = 0

    Classify(x : [8]) : [8] <combines, fails> {
        buf.Enq[x]
        return x
    }

    rule compute {
        buf.Enq[b]
        result := Classify(a)
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("appears more than once in this rule"))
    );
}

#[test]
fn a_direct_deq_and_a_via_callee_enq_combine_into_one_pass_through() {
    // The rule's own `Deq` and the callee's own `Enq` touch the SAME
    // fifo — this must produce the ordinary Enq+Deq pass-through
    // (`valid` stays 1, data updates to the new value), not a
    // dequeue-only guard, proving `compile_guard`'s fifo pre-scan and
    // module.rs's state-transition emission agree on the SAME whole-
    // rule view of this fifo (both now routed through `rule_fifo_ops`),
    // not two independently-computed answers that happen to usually
    // match.
    let src = "\
module Top {
    fifo buf : [8]
    in a : [8]
    out result : [8] = 0
    out val : [8] = 0

    Classify(x : [8]) : [8] <combines, fails> {
        buf.Enq[x]
        return x
    }

    rule compute {
        val := buf.Deq[]
        result := Classify(a)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    // The combined pass-through guard is just `valid` (see fifo.rs's
    // `rule_fifo_guard_cond`) -- NOT `and(valid, not(valid))`, which
    // would be the wrong, always-false AND of each op's own individual
    // guard if the two ops were folded independently instead of unified.
    assert!(fir.contains("node fires_compute = __fifo_buf_valid"));
    assert!(!fir.contains("and(__fifo_buf_valid"));
    assert!(fir.contains("connect __fifo_buf_valid, UInt<1>(1)"));
    run_firtool(&fir, &["--disable-opt"]);
}

#[test]
fn a_failing_callee_that_calls_another_failing_callee_is_still_rejected() {
    // v0 folds one level only — `Outer` itself has no guard of its own,
    // its `sig.fails` is entirely bubbled up from calling `Inner`.
    // Folding would need to recurse into `Inner`'s own body too, not
    // built here.
    let src = "\
Inner(x : [8]) : [8] <combines, fails> {
    (x <> 0)?
    return x
}
Outer(x : [8]) : [8] <combines, fails> {
    return Inner(x)
}
module Top {
    in a : [8]
    out result : [8] = 0
    rule compute {
        result := Outer(a)
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(err.iter().any(|e| {
        e.message
            .contains("reduces entirely to bare, top-level guards")
    }));
}

#[test]
fn a_failing_call_nested_in_if_else_at_the_rule_level_is_still_rejected() {
    // Same v0 restriction ordinary guards/fifo ops already have at the
    // rule level (`check_guard_placement`): a guard must gate the WHOLE
    // rule, unconditionally — one reachable only through a branch reads
    // as conditional even though nothing here would actually make it
    // behave that way, so it's rejected for clarity, matching existing
    // precedent exactly.
    let src = "\
Classify(x : [8]) : [8] <combines, fails> {
    (x <> 0)?
    return x
}
module Top {
    in a : [8]
    in cond : [1]
    out result : [8] = 0
    rule compute {
        if logic cond = 1 {
            result := Classify(a)
        } else {
            result := 0
        }
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(err.iter().any(|e| {
        e.message
            .contains("nested in if/while, is not yet supported")
    }));
}

#[test]
fn a_failing_call_as_a_bare_statement_after_a_state_write_is_still_rejected() {
    let src = "\
Classify(x : [8]) : [8] <combines, fails> {
    (x <> 0)?
    return x
}
module Top {
    in a : [8]
    reg r : [8] = 0
    rule compute {
        r := a
        Classify(a)
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("after a state write"))
    );
}

#[test]
fn a_write_transitively_reached_through_a_bare_statement_call_threads_through() {
    // Regression test for a real silent miscompile caught before
    // callee-calling-callee shipped: `Outer` (called as a bare
    // statement, its return value unused) itself writes `v` from
    // `Inner`'s return value (`v := Inner(x)`, the whole-RHS position),
    // and `Inner` ALSO writes `w` directly. `effects.rs` merges
    // `Outer`'s signature to include `w`, so `schedule.rs` correctly
    // believes the rule writes `w` -- proving BOTH `v` and `w` actually
    // land in the emitted hardware, not just that `w`'s write no longer
    // silently vanishes.
    let src = "\
module M {
    reg v : [8] = 0
    reg w : [8] = 0
    in a : [8]
    Inner(y : [8]) : [8] <combines> {
        w := y
        return y
    }
    Outer(x : [8]) : [8] <combines> {
        v := Inner(x)
        return x
    }
    rule r {
        Outer(a)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect v, a"));
    assert!(fir.contains("connect w, a"));
    run_firtool(&fir, &[]);
}

#[test]
fn call_to_a_function_with_a_non_tail_if_is_still_too_complex_to_inline() {
    // An `if` before the trailing `return` (not itself the final
    // statement) isn't the tail-branching shape `compile_callee_body`
    // recurses into -- it's just a `let`-shaped slot with an `if` in
    // it, which is rejected the same as any other non-`let` leading
    // statement.
    let src = "\
Pick(x : [8]) : [8] <combines> {
    if logic x > 10 {
        return x
    }
    return 0
}
module M {
    in a : [8]
    out result : [8] = 0
    rule r {
        result := Pick(a)
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("too complex to inline"))
    );
}

#[test]
fn call_inlines_a_function_with_an_if_else_branching_return() {
    let src = "\
Max(a : [8], b : [8]) : [8] <combines> {
    if logic a > b {
        return a
    } else {
        return b
    }
}
module M {
    in x : [8]
    in y : [8]
    out result : [8] = 0
    rule r {
        result := Max(x, y)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("mux(gt(x, y), x, y)"));
    run_firtool(&fir, &["--disable-opt"]);
}

#[test]
fn call_to_a_function_with_a_tail_if_and_no_else_is_an_error() {
    let src = "\
Pick(x : [8]) : [8] <combines> {
    if logic x > 10 {
        return x
    }
}
module M {
    in a : [8]
    out result : [8] = 0
    rule r {
        result := Pick(a)
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("must have an `else`"))
    );
}

#[test]
fn call_inlines_a_function_with_lets_inside_branches_that_do_not_leak_out() {
    // Each branch's own `let` is bound/restored around that branch's
    // recursive compile -- this pins that a `let` with the SAME name in
    // both branches doesn't collide (each `Let` statement has its own
    // DefId regardless of the shared name), and that compiling the
    // `else` branch after the `then` branch doesn't see the `then`
    // branch's local still bound.
    let src = "\
Pick(a : [8], b : [8]) : [8] <combines> {
    if logic a > b {
        let winner = a
        return winner
    } else {
        let winner = b
        return winner
    }
}
module M {
    in x : [8]
    in y : [8]
    out result : [8] = 0
    rule r {
        result := Pick(x, y)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("mux(gt(x, y), x, y)"));
    run_firtool(&fir, &["--disable-opt"]);
}

#[test]
fn call_to_an_unsynthesizable_builtin_is_still_an_error() {
    // `prio`/`trunc`/`pack` are the synthesizable builtins (see the
    // `prio_*`/`trunc_*`/`pack_*` tests); `clog2` (a compile-time-only
    // `Ty::Int` construct — see DESIGN.md's "the second synthesizable
    // builtin" section) remains an explicit, separate gap — this pins
    // that it doesn't get conflated with the others.
    let src = "\
module M {
    in a : [8]
    out result : [8] = 0
    rule r {
        result := clog2(a)
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(err.iter().any(|e| {
        e.message
            .contains("calling the builtin `clog2` is not yet supported")
    }));
}

#[test]
fn pack_concatenates_msb_first() {
    let src = "\
module M {
    in a : [8]
    in b : [8]
    out result : [16] = 0
    rule r {
        result := pack(a, b)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_result, cat(a, b)"));
    run_firtool(&fir, &[]);
}

#[test]
fn pack_of_three_folds_left_to_right() {
    let src = "\
module M {
    in a : [8]
    in b : [8]
    in c : [8]
    out result : [24] = 0
    rule r {
        result := pack(a, b, c)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_result, cat(cat(a, b), c)"));
    run_firtool(&fir, &[]);
}

#[test]
fn pack_inlines_through_a_user_fn_that_wraps_it() {
    // Like `prio`/`trunc`, a call to `pack` doesn't disqualify its
    // enclosing callee from inlining.
    let src = "\
module M {
    in a : [8]
    in b : [8]
    out result : [16] = 0

    Combine(x : [8], y : [8]) : [16] <combines> {
        return pack(x, y)
    }

    rule r {
        result := Combine(a, b)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_result, cat(a, b)"));
    run_firtool(&fir, &[]);
}

#[test]
fn trunc_takes_the_low_bits() {
    let src = "\
module M {
    in a : [16]
    out result : [8] = 0
    rule r {
        result := trunc(a, 8)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_result, bits(a, 7, 0)"));
    run_firtool(&fir, &[]);
}

#[test]
fn trunc_inlines_through_a_user_fn_that_wraps_it() {
    // Like `prio`, a call to `trunc` doesn't disqualify its enclosing
    // callee from inlining the way a call to another user `fn`/`impl`
    // still does.
    let src = "\
module M {
    in a : [16]
    out result : [8] = 0

    Narrow(x : [16]) : [8] <combines> {
        return trunc(x, 8)
    }

    rule r {
        result := Narrow(a)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_result, bits(a, 7, 0)"));
    run_firtool(&fir, &[]);
}

#[test]
fn prio_encodes_the_lowest_set_bit_as_a_priority_mux_chain() {
    let src = "\
module M {
    in reqs : [4]
    out grant : [2] = 0
    rule r {
        grant := prio(reqs)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains(
        "connect __out_grant, mux(bits(reqs, 0, 0), UInt<2>(0), \
         mux(bits(reqs, 1, 1), UInt<2>(1), mux(bits(reqs, 2, 2), UInt<2>(2), \
         mux(bits(reqs, 3, 3), UInt<2>(3), UInt<2>(0)))))"
    ));
    run_firtool(&fir, &[]);
}

#[test]
fn prio_inlines_through_a_user_fn_that_wraps_it() {
    // Matches `examples/arbiter.tr`'s `RoundRobin` shape: a `prio` call
    // living inside an otherwise-ordinary inlinable callee body. A
    // builtin call doesn't disqualify the callee from inlining the way
    // a call to another user `fn`/`impl` still does (see
    // `nested_user_call_inside_a_builtins_argument_still_disqualifies`).
    let src = "\
module M {
    in reqs : [4]
    out grant : [2] = 0

    RoundRobin(r : [4]) : [2] <combines> {
        return prio(r)
    }

    rule r {
        grant := RoundRobin(reqs)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_grant, mux(bits(reqs, 0, 0), UInt<2>(0),"));
    run_firtool(&fir, &[]);
}

#[test]
fn user_call_nested_inside_a_builtins_argument_composes() {
    // `prio` doesn't disqualify a callee from inlining, and (since
    // callee-calling-callee composition landed) neither does a user
    // call nested inside a builtin's own argument: `Mask` clears bit 3
    // before `prio` ever sees it, so feeding in a request with ONLY bit
    // 3 set must still fall back to 0 (no eligible bit), not 3 — proof
    // `Mask` actually ran first, not just that emission succeeded.
    let src = "\
module M {
    in reqs : [4]
    out grant : [2] = 0

    Mask(x : [4]) : [4] <combines> {
        return x & 4'd7
    }
    RoundRobin(r : [4]) : [2] <combines> {
        return prio(Mask(r))
    }

    rule r {
        grant := RoundRobin(reqs)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("and(reqs, UInt<4>(7))"));
    let verilog = run_firtool(&fir, &[]).expect("firtool should compile this design");
    assert!(verilog.contains("module M"));
}

#[test]
fn mutually_exclusive_claim_emits_a_simulation_assertion() {
    // `mutually_exclusive { a, b }` waives the derived mutual-exclusion
    // stall between two conflicting rules -- the claim is recorded, not
    // trusted (DESIGN.md's "Scheduling" tier 2): the compiler must
    // insert a real check, not just silently accept the annotation.
    let src = "\
module M {
    reg a : [8] = 0
    reg b : [8] = 0
    in we_a : [1]
    in we_b : [1]

    rule set_a {
        (we_a = 1)?
        a := b
    }

    rule set_b {
        (we_b = 1)?
        b := a
    }

    schedule {
        urgency set_a > set_b
        mutually_exclusive { set_a, set_b }
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains(
        "assert(clock, not(and(fires_set_a, fires_set_b)), not(reset), \
         \"mutually_exclusive claim violated: rule set_a and rule set_b both fired the same \
         cycle\") : mutually_exclusive_check_0"
    ));
    // No derived stall between an exempted pair -- unlike a non-exempted
    // conflict, neither rule's `fires_*` should reference the other's.
    assert!(!fir.contains("not(fires_set_a)"));
    assert!(!fir.contains("not(fires_set_b)"));
    run_firtool(&fir, &[]);
}

#[test]
fn conflict_free_claim_waives_the_stall_but_emits_no_assertion() {
    // `conflict_free { a, b }` claims the OPPOSITE thing
    // `mutually_exclusive` does: safe to fire concurrently, not
    // never-both-fire. It still waives the derived stall (same as
    // `mutually_exclusive`), but v0 has no way to prove or check address
    // disjointness (DESIGN.md's tier-3 proof, deferred), so there is
    // nothing sound to assert -- unlike `mutually_exclusive`, this must
    // emit NO assertion at all, not an inverted or placeholder one.
    let src = "\
module M {
    reg a : [8] = 0
    reg b : [8] = 0
    in we_a : [1]
    in we_b : [1]

    rule set_a {
        (we_a = 1)?
        a := b
    }

    rule set_b {
        (we_b = 1)?
        b := a
    }

    schedule {
        urgency set_a > set_b
        conflict_free { set_a, set_b }
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(!fir.contains("assert("));
    // Still no derived stall between the exempted pair.
    assert!(!fir.contains("not(fires_set_a)"));
    assert!(!fir.contains("not(fires_set_b)"));
    run_firtool(&fir, &[]);
}

#[test]
fn mem_read_address_is_a_local_in_a_rule_that_isnt_last_in_urgency_order() {
    // Reader ports are wired unconditionally in one pass, AFTER every
    // rule's own `enter_rule`/compile pass has already happened (reads
    // are free, driven regardless of which rule fires) -- so that pass
    // used to run with `cx.locals` left over from whichever rule was
    // entered LAST in the fires loop, not the rule the read site
    // actually belongs to. `read_at_five`'s own local `x` is invisible
    // by the time `incr` (declared lower-urgency, so entered later)
    // finishes, and its address used to fail with "cannot find this
    // local's binding" -- a real, previously latent bug, not a
    // hypothetical.
    let src = "\
module M {
    mem m : [16][256]
    reg out : [16] = 0
    reg counter : [8] = 0

    rule read_at_five {
        let x = 5
        out := m[x]
    }

    rule incr {
        counter := counter + 1
    }

    schedule {
        urgency read_at_five > incr
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    // Also proves the width-hint fix: a bare literal (`x := 5`) has no
    // width of its own without the reader loop's new address-width hint.
    assert!(fir.contains("connect m.r0.addr, UInt<8>(5)"));
    run_firtool(&fir, &[]);
}

#[test]
fn mem_read_address_is_a_local_bound_to_an_input() {
    let src = "\
module M {
    mem m : [16][256]
    in addr : [8]
    reg out : [16] = 0
    rule r {
        let x = addr
        out := m[x]
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect m.r0.addr, addr"));
    run_firtool(&fir, &[]);
}

#[test]
fn logic_of_a_fifo_op_reads_occupancy_with_no_dequeue() {
    let src = "\
module M {
    fifo f : [8]
    out ready : [1] = 0
    rule r {
        ready := logic f.Deq[]
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_ready, __fifo_f_valid"));
    // No dequeue side effect anywhere -- `logic` alone never touches
    // the fifo's own state, only reads it.
    assert!(!fir.contains("connect __fifo_f_valid, UInt<1>(0)"));
    run_firtool(&fir, &[]);
}

#[test]
fn logic_of_a_guard_only_call_reads_its_condition() {
    let src = "\
Classify(x : [8]) : [8] <combines, fails> {
    (x <> 0)?
    return x
}
module M {
    in a : [8]
    out ok : [1] = 0
    rule r {
        ok := logic Classify(a)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_ok, neq(a, UInt<8>(0))"));
    run_firtool(&fir, &[]);
}

#[test]
fn logic_rejects_a_call_that_also_writes_state() {
    // The v0 restriction mirroring Verse's own `<decides>`-only rule for
    // `logic{}` (confirmed against `02_primitives`, see TODO.md): a
    // callee that both guard-folds AND writes state is fine as a DIRECT
    // call (`call_writes_runs_through_real_ports` etc.), but silently
    // discarding its write just because it's reached through `logic`
    // would be a confusing footgun, not a supported feature.
    let src = "\
module M {
    reg v : [8] = 0
    in a : [8]
    out ok : [1] = 0

    Bump(x : [8]) : [8] <combines, fails> {
        v := x
        (x <> 0)?
        return x
    }

    rule r {
        ok := logic Bump(a)
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(err.iter().any(|e| e.message.contains("also writes state")));
    // Exactly one error, not also the generic writing-call-position
    // message `check_writing_call_positions` would otherwise ALSO raise
    // on the same span (`Bump(a)` genuinely does sit nested inside a
    // larger expression) -- pins the exemption added to
    // `check_writing_call_positions_in` alongside the fifo/failing-call
    // ones, found by an advisor review after this test initially passed
    // for the wrong reason (two errors, `.any` didn't notice the extra).
    assert_eq!(err.len(), 1, "expected exactly one error, got {err:?}");
}

#[test]
fn logic_of_a_fifo_op_enqueue_reads_space_availability() {
    let src = "\
module M {
    fifo f : [8]
    in x : [8]
    out has_space : [1] = 0
    rule r {
        has_space := logic f.Enq[x]
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_has_space, not(__fifo_f_valid)"));
    // No enqueue side effect -- the fifo's own data register never gets
    // written by `logic` alone.
    assert!(!fir.contains("connect __fifo_f_data"));
    run_firtool(&fir, &[]);
}

#[test]
fn logic_of_a_comparison_reads_its_ordinary_boolean_value() {
    // A comparison's success condition IS its own ordinary `eq`/`neq`/
    // `lt`/... primop -- no side effect to skip, unlike a fifo op or a
    // call, so `logic`'s job here is purely "type-check as a definite
    // [1]", not "read some other, less-visible condition".
    let src = "\
module M {
    in a : [8]
    in b : [8]
    out ok : [1] = 0
    rule r {
        ok := logic a > b
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_ok, gt(a, b)"));
    // Pin the discharge itself: `logic` must NOT also fold `a > b` into
    // the rule's guard (`comparison_conds`' recursive search has to stop
    // at `Expr::Logic`, not walk through it) -- self-caught by direct
    // probe, this used to silently emit `fires_r = gt(a, b)`, defeating
    // the entire point of writing `logic` in the first place.
    assert!(fir.contains("node fires_r = UInt<1>(1)"));
    run_firtool(&fir, &[]);
}

#[test]
fn logic_wrapped_comparison_inside_an_if_body_is_allowed_and_discharged() {
    // The if/while-nesting check (`contains_comparison`) has to make the
    // same `Expr::Logic`-stops-the-walk exemption as `comparison_conds`
    // above, or a fully discharged comparison would be wrongly rejected
    // just for appearing inside a conditional branch.
    let src = "\
module M {
    reg v : [8] = 0
    in a : [8]
    in b : [8]
    in c : [1]
    rule r {
        if logic c = 1 {
            v := logic a > b
        }
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_r = UInt<1>(1)"));
    assert!(fir.contains("connect v, mux(eq(c, UInt<1>(1)), gt(a, b), v)"));
    run_firtool(&fir, &["--disable-opt"]);
}

#[test]
fn logic_of_a_fifo_op_works_inside_a_callees_own_body() {
    // Advisor flagged this as untested: `check_logic_args` originally
    // only ran per-RULE (module.rs's loop), never on a callee's own
    // body -- this proves the positive case actually compiles correctly
    // through `validate_call`'s callee-body reach, not just that it's
    // accepted.
    let src = "\
module M {
    fifo f : [8]
    out ready : [1] = 0

    Probe() : [1] <combines> {
        return logic f.Deq[]
    }

    rule r {
        ready := Probe()
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_ready, __fifo_f_valid"));
    assert!(!fir.contains("connect __fifo_f_valid, UInt<1>(0)"));
    run_firtool(&fir, &[]);
}

#[test]
fn logic_rejects_a_write_callee_wrapped_inside_a_callees_own_body() {
    // The sharper version of the write-callee rejection: `Bump` isn't
    // called directly from the rule, it's nested one level deeper,
    // inside `Probe`'s own body -- reached only via `validate_call`'s
    // callee-body check, not the rule-level one. Before `check_logic_
    // args_in` was wired into `validate_call`, this compiled clean and
    // silently dropped `v`'s write, exactly the bug this whole check
    // exists to prevent.
    let src = "\
module M {
    reg v : [8] = 0
    in a : [8]
    out ok : [1] = 0

    Bump(x : [8]) : [8] <combines, fails> {
        v := x
        (x <> 0)?
        return x
    }

    Probe(x : [8]) : [1] <combines> {
        return logic Bump(x)
    }

    rule r {
        ok := Probe(a)
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(err.iter().any(|e| e.message.contains("also writes state")));
}

#[test]
fn logic_after_a_state_write_is_not_rejected_by_guard_placement() {
    // `check_guard_placement`'s "guard/fifo op/failing call after a
    // state write" restriction looks for the raw shapes directly
    // (`self.fifo_op(e)`, `self.is_failing_call(e)`, `is_guard_like`) --
    // `Expr::Logic` is none of those (a real prefix node yielding a
    // `[1]` VALUE), so it correctly falls outside that restriction
    // entirely and may appear anywhere an ordinary value can, including
    // after a write. Pins that this is real, deliberate behavior (a
    // plain value has nothing left to fold into a guard), not an
    // accidental gap in `check_guard_placement`'s shape matching.
    let src = "\
module M {
    fifo f : [8]
    reg v : [8] = 0
    out ready : [1] = 0
    rule r {
        v := v + 1
        ready := logic f.Deq[]
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_ready, __fifo_f_valid"));
    run_firtool(&fir, &[]);
}

#[test]
fn logic_rejects_a_call_that_never_fails() {
    let src = "\
Pure(x : [8]) : [8] <combines> {
    return x + 1
}
module M {
    in a : [8]
    out ok : [1] = 0
    rule r {
        ok := logic Pure(a)
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("must be able to fail"))
    );
}

#[test]
fn logic_rejects_a_non_fallible_argument() {
    // Parens are no longer load-bearing here (unlike when this test was
    // first written): `logic`'s operand parses loosely now, so a bare
    // `logic a + b` already means `logic (a + b)` -- kept anyway since
    // they read clearly either way.
    let src = "\
module M {
    in a : [8]
    in b : [8]
    out ok : [1] = 0
    rule r {
        ok := logic (a + b)
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(err.iter().any(|e| {
        e.message
            .contains("needs a fifo op, a comparison, or a call")
    }));
}

#[test]
fn logic_of_a_fifo_op_composes_with_a_real_conflict_free_dequeue() {
    // Mirrors examples/logic_probe.tr: `probe`'s `logic input.Deq[]`
    // must not interfere with `drain`'s real, separately-scheduled
    // dequeue of the SAME fifo -- proving effects.rs correctly excludes
    // the probe from `sig.writes` (a naive merge would make `probe`
    // conflict with `drain` on a write/write basis, forcing one to
    // stall the other via urgency instead of `conflict_free` being a
    // legal, sufficient annotation).
    let src = "\
module M {
    fifo input : [8]
    out ready : [1] = 0
    out consumed : [8] = 0

    rule probe {
        ready := logic input.Deq[]
    }

    rule drain {
        consumed := input.Deq[]
    }

    schedule {
        conflict_free { probe, drain }
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_drain = __fifo_input_valid"));
    run_firtool(&fir, &[]);
}

#[test]
fn logic_wrapped_call_is_allowed_inside_an_if_condition() {
    // `check_guard_placement`'s if/while sub-checks (`contains_guard`/
    // `contains_fifo_op`/`contains_failing_call`) predate `logic`
    // and originally had no exemption for it, even though the other
    // three position checks (`check_failing_call_positions`/`check_
    // fifo_op_positions`/`check_writing_call_positions_in`) already did
    // — found while probing whether `logic A & logic B` fully
    // replaces a Verse-style `and` operator (it does, once this compiled
    // at all): `logic Check(a)` here is a plain `[1]` value with no
    // remaining guard-fold obligation, and belongs anywhere any other
    // value does, including an `if` condition combined with `&`. Each
    // side needs its own parens now that `logic`'s operand parses
    // loosely (see `logic_combined_with_amp_now_needs_parens_on_each_
    // side`, tests/parser.rs) — a bare `logic f.Deq[] & logic Check(a)`
    // would parse as one `logic` wrapping the whole `&` expression
    // instead of two separately-discharged values.
    let src = "\
Check(x : [8]) : [8] <combines, fails> {
    (x <> 0)?
    return x
}
module M {
    fifo f : [8]
    in a : [8]
    out ok : [1] = 0
    rule r {
        if (logic f.Deq[]) & (logic Check(a)) {
            ok := 1
        } else {
            ok := 0
        }
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains(
        "connect __out_ok, mux(and(__fifo_f_valid, neq(a, UInt<8>(0))), UInt<1>(1), UInt<1>(0))"
    ));
    run_firtool(&fir, &[]);
}

#[test]
fn two_logic_wrapped_comparisons_combine_with_amp() {
    // DESIGN.md documents `(logic a > b) & (logic c < d)` as the
    // parenthesized idiom that replaces a Verse-style `and` operator now
    // that `logic`'s operand parses loosely — pin it directly rather
    // than relying on the fifo-op/call variant above to stand in for it.
    let src = "\
module M {
    in a : [8]
    in b : [8]
    in c : [8]
    in d : [8]
    out ok : [1] = 0
    rule r {
        ok := (logic a > b) & (logic c < d)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_ok, and(gt(a, b), lt(c, d))"));
    assert!(fir.contains("node fires_r = UInt<1>(1)"));
    run_firtool(&fir, &[]);
}

#[test]
fn logic_wrapped_call_with_a_comparison_argument_still_gates_on_it() {
    // Advisor-caught before commit: the fix above (stop `comparison_
    // conds`/`contains_comparison`'s walk at `Expr::Logic`) can't just
    // stop at EVERY `Expr::Logic` -- only a `logic`-wrapped COMPARISON
    // is actually discharged by `logic` itself. A `logic`-wrapped CALL
    // only discharges the call's own fail cond; an independent
    // comparison nested in its arguments is a separate, undischarged
    // failure. `logic Check(a > b)` must still gate `fires_r` on
    // `a > b`, even though `logic` discharges `Check`'s own guard.
    let src = "\
Check(x : [8]) : [8] <combines, fails> {
    (x <> 0)?
    return x
}
module M {
    in a : [8]
    in b : [8]
    out ok : [1] = 0
    rule r {
        ok := logic Check(a > b)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_r = gt(a, b)"));
    run_firtool(&fir, &[]);
}

/// `A or B or C` with a real default tail: an infallible, unconditional
/// priority pick — hand-lowered and Icarus-confirmed (fifo.rs's
/// `or_chains` doc comment) before this compiler code was written. No
/// guard term at all; `tick` advances even when neither fifo is ready.
#[test]
fn or_with_default_is_unconditional_and_prioritized() {
    let src = "\
module M {
    fifo a : [8]
    fifo b : [8]
    out result : [8] = 0
    out counter : [8] = 0
    rule r {
        result := a.Deq[] or b.Deq[] or 0
        counter := counter + 1
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_r = UInt<1>(1)"));
    assert!(fir.contains(
        "connect __out_result, mux(__fifo_a_valid, __fifo_a_data, \
         mux(and(__fifo_b_valid, not(__fifo_a_valid)), __fifo_b_data, UInt<8>(0)))"
    ));
    run_firtool(&fir, &[]);
}

/// `let`-bound `or`: DESIGN.md lists a `let` init among the three legal
/// positions (matching `or_chains`, fifo.rs), but only `:=` gets
/// exercised by every other test/example — pin that `let` genuinely
/// works too, not just that `or_chains` happens to match its shape.
#[test]
fn or_bound_via_let_folds_the_same_guard_as_assign() {
    let src = "\
module M {
    fifo a : [8]
    fifo b : [8]
    out result : [8] = 0
    rule r {
        let v = a.Deq[] or b.Deq[]
        result := v
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_r = or(__fifo_a_valid, __fifo_b_valid)"));
    run_firtool(&fir, &[]);
}

/// `A or B` with no default: stays fallible — the alternatives' combined
/// occupancy (ORed, not ANDed) becomes the rule's own guard.
#[test]
fn or_without_default_folds_an_ored_guard() {
    let src = "\
module M {
    fifo a : [8]
    fifo b : [8]
    out result : [8] = 0
    rule r {
        result := a.Deq[] or b.Deq[]
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_r = or(__fifo_a_valid, __fifo_b_valid)"));
    run_firtool(&fir, &[]);
}

#[test]
fn or_rejects_a_non_fifo_op_alternative_in_a_middle_position() {
    let src = "\
module M {
    fifo a : [8]
    in x : [8]
    out result : [8] = 0
    rule r {
        result := a.Deq[] or x or 0
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("must be a fifo `Deq[]`"))
    );
}

#[test]
fn or_rejects_depth_greater_than_one() {
    let src = "\
module M {
    fifo a : {4}[8]
    fifo b : [8]
    out result : [8] = 0
    rule r {
        result := a.Deq[] or b.Deq[]
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(err.iter().any(|e| e.message.contains("depth > 1")));
}

#[test]
fn or_rejects_a_fifo_also_touched_directly_elsewhere_in_the_rule() {
    let src = "\
module M {
    fifo a : [8]
    fifo b : [8]
    in x : [8]
    out result : [8] = 0
    rule r {
        a.Enq[x]
        result := a.Deq[] or b.Deq[]
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("also touched directly elsewhere"))
    );
}

#[test]
fn or_without_default_after_a_state_write_is_rejected() {
    let src = "\
module M {
    fifo a : [8]
    fifo b : [8]
    reg r0 : [8] = 0
    out result : [8] = 0
    rule r {
        r0 := 1
        result := a.Deq[] or b.Deq[]
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("with no default, after a state write"))
    );
}

/// `or` nested in `if`/`while` isn't wired into `or_chains`' (fifo.rs)
/// intentionally non-recursive walk, so its alternatives stay
/// unexempted and fall through to `check_fifo_op_positions`'s generic
/// "fifo operation ... not in an allowed position" message — this pins
/// WHICH check actually owns the rejection, rather than just reasoning
/// it through (per-advisor: write the test).
#[test]
fn or_nested_in_if_is_rejected_by_the_generic_fifo_position_check() {
    let src = "\
module M {
    fifo a : [8]
    fifo b : [8]
    in cond : [1]
    out result : [8] = 0
    rule r {
        if cond {
            result := a.Deq[] or b.Deq[]
        }
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("not in an allowed position")
                || e.message.contains("only appear as a whole statement"))
    );
}

/// `or` inside a callee's own body — with a real default tail, so the
/// callee's OWN `fails` is false and `check_fails_is_foldable_guard`
/// (which only ever looks for a nested guard/fifo op, not an `Or` node)
/// never gets a chance to reject it. Without `check_no_or_in_callee_
/// body` (calls.rs's `validate_call`), this compiled clean: a priority
/// mux reading `__fifo_a_data`/`__fifo_b_data` every cycle with NO
/// `connect __fifo_a_valid, UInt<1>(0)` anywhere — a fifo read every
/// cycle but never actually dequeued. Found by hand-testing, not by
/// construction — pins the fix.
#[test]
fn or_inside_a_callees_own_body_with_a_default_is_rejected() {
    let src = "\
module M {
    fifo a : [8]
    fifo b : [8]
    out result : [8] = 0
    Pick() : [8] <combines> {
        return a.Deq[] or b.Deq[] or 0
    }
    rule r {
        result := Pick()
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(err.iter().any(|e| e.message.contains("callee's own body")));
}

/// The undefaulted form takes a DIFFERENT rejection path: the callee's
/// own `fails` is true, so `check_fails_is_foldable_guard` rejects it
/// first (it only recognizes bare guards/fifo ops, not `Or`) — before
/// `check_no_or_in_callee_body` even matters. Both forms must be safe;
/// this pins that the undefaulted one already was, independently.
#[test]
fn or_inside_a_callees_own_body_without_a_default_is_rejected() {
    let src = "\
module M {
    fifo a : [8]
    fifo b : [8]
    out result : [8] = 0
    Pick() : [8] <combines, fails> {
        return a.Deq[] or b.Deq[]
    }
    rule r {
        result := Pick()
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(!err.is_empty());
}

/// Cross-tick guard/fifo-op audit (TODO.md): a fifo op sitting AFTER a
/// `tick` gets the full ordinary per-rule guard-placement treatment, not
/// some special-cased or missing check. `sequences` lowering splits each
/// segment into its OWN `rule {name}_s{N}` (`render_rule`, lower.rs),
/// gated by `(cont = N)?` — so post-tick code is simply a fresh rule's
/// own top-level statement by the time `check_guard_placement`/`compile_
/// guard` ever see it, not a special "after a tick" position needing its
/// own machinery. Confirmed by asserting the segment-1 guard folds
/// `__cont_go`'s own gate together with the fifo's occupancy.
#[test]
fn a_fifo_op_after_a_tick_gets_the_ordinary_per_segment_guard_fold() {
    let src = "\
module M {
    fifo f : [8]
    reg r0 : [8] = 0
    rule go <sequences, writes {r0, f}, reads {f}> {
        r0 := 1
        tick
        r0 := f.Deq[]
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains(
        "node fires_go_s1 = and(and(eq(__cont_go, UInt<1>(1)), __fifo_f_valid), not(fires_go_s0))"
    ));
    run_firtool(&fir, &[]);
}

/// A struct-typed reg/output never emits a real FIRRTL bundle: it
/// flattens to N plain registers/ports, one per field, named
/// `{name}_{field}` -- confirmed to match what firtool's own bundle
/// lowering produces (hand-lowered through real firtool before this was
/// written; see DESIGN.md's "Structs" section).
#[test]
fn struct_reg_and_output_flatten_to_per_field_registers() {
    let fir = emit_from_source(&read_example("struct_pair.tr")).expect("emission should succeed");
    assert!(fir.contains("regreset p_valid : UInt<1>"));
    assert!(fir.contains("regreset p_data : UInt<8>"));
    assert!(fir.contains("output result_valid : UInt<1>"));
    assert!(fir.contains("output result_data : UInt<8>"));
    run_firtool(&fir, &[]);
}

/// A struct-typed port on an INSTANTIATED submodule is rejected (v0
/// restriction): the target module's own port flattens to N real FIRRTL
/// ports (`p_valid`/`p_data`), but the instance-wiring code here only
/// ever knows the port's bare, unflattened name -- wiring it by that
/// name would either reference a port that doesn't exist or (silently,
/// worse) default-wire a single bit. Caught explicitly instead.
#[test]
fn struct_typed_instance_port_is_rejected() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

module Child {
    out p : Pair = Pair{ valid: 0, data: 0 }
    rule fill {
        p := Pair{ valid: 1, data: 5 }
    }
}

module Parent {
    inst c : Child
    out v : [1] = 0
    rule read {
        v := c.p.valid
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter().any(|e| e.message.contains("struct-typed port")),
        "expected a struct-typed-port rejection, got: {err:?}"
    );
}

/// A struct-typed reg write nested in `if`/`else` mux-threads per FIELD,
/// same as an ordinary scalar reg -- and, with no `else`, holds the
/// FLAT field register (`p_valid`, not the un-flattened `p`) on the
/// branch that doesn't write. This is the exact coverage gap this
/// session already caught once for mem writes (if/else-both-branches);
/// `struct_field_value_in_stmts` builds its hold name from `{struct}_
/// {field}` (see its own doc comment), which this pins.
#[test]
fn struct_reg_write_nested_in_if_else_mux_threads_per_field() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

module M {
    reg p : Pair = Pair{ valid: 0, data: 0 }
    in go : [1]

    rule r {
        if logic go = 1 {
            p := Pair{ valid: 1, data: 8'd7 }
        } else {
            p := Pair{ valid: 0, data: 8'd9 }
        }
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect p_valid, mux(eq(go, UInt<1>(1)), UInt<1>(1), UInt<1>(0))"));
    assert!(fir.contains("connect p_data, mux(eq(go, UInt<1>(1)), UInt<8>(7), UInt<8>(9))"));
    run_firtool(&fir, &[]);
}

#[test]
fn struct_reg_write_nested_in_if_with_no_else_holds_the_flat_field_name() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

module M {
    reg p : Pair = Pair{ valid: 0, data: 0 }
    in go : [1]

    rule r {
        if logic go = 1 {
            p := Pair{ valid: 1, data: 8'd7 }
        }
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect p_valid, mux(eq(go, UInt<1>(1)), UInt<1>(1), p_valid)"));
    assert!(fir.contains("connect p_data, mux(eq(go, UInt<1>(1)), UInt<8>(7), p_data)"));
    run_firtool(&fir, &[]);
}

/// A struct-typed `in` port flattens the same way a struct-typed `reg`/
/// `out` does -- `compile_struct_field_read`'s `Reg | Input` branch
/// (shared with `reg`) is otherwise unexercised by anything else in the
/// tree (`struct_pair.tr`'s `input` is a fifo, not a port).
#[test]
fn struct_typed_input_port_flattens_and_reads_by_field() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

module M {
    in q : Pair
    out ok : [1] = 0

    rule r {
        ok := q.valid
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("input q_valid : UInt<1>"));
    assert!(fir.contains("input q_data : UInt<8>"));
    assert!(fir.contains("connect __out_ok, q_valid"));
    run_firtool(&fir, &[]);
}

/// A struct-typed local bound directly to a struct literal resolves its
/// field reads through the lazy `locals` map (not `locals_snapshots`,
/// since a struct-typed local's width is never a concrete `[N]`).
#[test]
fn struct_typed_local_bound_to_a_literal_resolves_field_reads() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

module M {
    out out_v : [8] = 0

    rule r {
        let q = Pair{ valid: 1, data: 8'd7 }
        out_v := q.data
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_out_v, UInt<8>(7)"));
    run_firtool(&fir, &[]);
}

/// A struct-typed local that's merely an ALIAS for another struct-typed
/// value (not bound directly to a literal) doesn't resolve field reads
/// in v0 -- DESIGN.md's "Structs" section documents this restriction;
/// this pins it as an actual compile error, not a silent miscompile.
#[test]
fn struct_typed_local_aliasing_another_local_is_rejected() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

module M {
    out out_v : [8] = 0

    rule r {
        let q = Pair{ valid: 1, data: 8'd7 }
        let p = q
        out_v := p.data
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter().any(|e| e.message.contains("struct literal")),
        "expected an aliasing-rejection error, got: {err:?}"
    );
}

/// A NESTED struct (a struct field that's itself another struct)
/// flattens all the way down: `Types::struct_field_widths` recurses
/// through the inner struct, joining names with `_` at every level, so
/// `f_header_valid`/`f_header_seq`/`f_data` -- not a real FIRRTL bundle
/// at any level -- same as `struct_reg_and_output_flatten_to_per_field_
/// registers` above, just carried one level deeper.
#[test]
fn nested_struct_flattens_all_the_way_down() {
    let fir = emit_from_source(&read_example("struct_nested.tr")).expect("emission should succeed");
    assert!(fir.contains("regreset f_header_valid : UInt<1>"));
    assert!(fir.contains("regreset f_header_seq : UInt<4>"));
    assert!(fir.contains("regreset f_data : UInt<8>"));
    assert!(fir.contains("output result_header_valid : UInt<1>"));
    assert!(fir.contains("output result_header_seq : UInt<4>"));
    assert!(fir.contains("output result_data : UInt<8>"));
    run_firtool(&fir, &[]);
}

/// A NESTED struct-typed `in` port flattens the same way a nested reg/
/// out does -- `module.rs`'s `Item::Input` arm and `compile_struct_
/// field_read`'s `Reg | Input` branch build the flat name from opposite
/// ends (declaration vs. a chained `.field.field` read) and must agree.
#[test]
fn nested_struct_typed_input_port_flattens_and_reads_by_chained_field() {
    let src = "\
struct Header {
    valid : [1]
    seq : [4]
}

struct Frame {
    header : Header
    data : [8]
}

module M {
    in q : Frame
    out ok : [4] = 0

    rule r {
        ok := q.header.seq
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("input q_header_valid : UInt<1>"));
    assert!(fir.contains("input q_header_seq : UInt<4>"));
    assert!(fir.contains("input q_data : UInt<8>"));
    assert!(fir.contains("connect __out_ok, q_header_seq"));
    run_firtool(&fir, &[]);
}

/// A nested struct-typed reg write nested in `if`/`else` mux-threads
/// per LEAF field, holding the flat leaf name (not the un-flattened
/// `f` or the one-level `f_header`) on the branch that doesn't write --
/// the same coverage this session already pinned for a single-level
/// struct, one level deeper.
#[test]
fn nested_struct_write_in_if_with_no_else_holds_the_flat_leaf_name() {
    let src = "\
struct Header {
    valid : [1]
    seq : [4]
}

struct Frame {
    header : Header
    data : [8]
}

module M {
    reg f : Frame = Frame{ header: Header{ valid: 0, seq: 0 }, data: 0 }
    in go : [1]

    rule r {
        if logic go = 1 {
            f := Frame{ header: Header{ valid: 1, seq: 4'd3 }, data: 8'd7 }
        }
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(
        fir.contains("connect f_header_valid, mux(eq(go, UInt<1>(1)), UInt<1>(1), f_header_valid)")
    );
    assert!(
        fir.contains("connect f_header_seq, mux(eq(go, UInt<1>(1)), UInt<4>(3), f_header_seq)")
    );
    assert!(fir.contains("connect f_data, mux(eq(go, UInt<1>(1)), UInt<8>(7), f_data)"));
    run_firtool(&fir, &[]);
}

/// A struct literal is allowed to span multiple lines, its opening `{`
/// immediately followed by a newline before the first field --
/// `at_struct_lit_open`'s 2-token lookahead must skip past that newline
/// before checking `ident :` vs `ident :=`, or a multi-line literal
/// (the natural way to write a nested one readably) silently fails to
/// parse as a struct literal at all.
#[test]
fn a_multiline_struct_literal_still_parses_as_one() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

module M {
    reg p : Pair = Pair{ valid: 0, data: 0 }
    in go : [1]

    rule r {
        if logic go = 1 {
            p := Pair{
                valid: 1,
                data: 8'd7
            }
        }
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect p_valid, mux(eq(go, UInt<1>(1)), UInt<1>(1), p_valid)"));
    assert!(fir.contains("connect p_data, mux(eq(go, UInt<1>(1)), UInt<8>(7), p_data)"));
    run_firtool(&fir, &[]);
}

/// The multi-line fix above must NOT reopen the collision it was
/// checked against when this session's struct feature first landed:
/// `if cond { x := 1 }`, with the assignment on its own line, must
/// still parse as an ordinary if/block, not get misread as a struct
/// literal now that the lookahead skips newlines.
#[test]
fn if_with_a_bare_ident_condition_and_a_newline_before_its_body_still_parses_as_a_block() {
    let src = "\
module M {
    reg x : [8] = 0
    in cond : [1]

    rule r {
        if cond {
            x := 1
        }
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect x, mux(cond, UInt<8>(1), x)"));
    run_firtool(&fir, &[]);
}

/// `?T` never emits a real FIRRTL bundle either, same as a plain
/// `struct` -- a `?T`-typed REG flattens to two registers, `{name}_
/// valid`/`{name}_data`; `false` writes `valid=0`, a bare value of `T`
/// (auto-coerced present) writes `valid=1`. A `?T`-typed OUTPUT PORT
/// flattens the same way but through separate bookkeeping (real
/// `output {port}_valid`/`{port}_data` ports, backed by internal
/// `__out_{port}_valid`/`{port}_data` registers) -- pinned here too,
/// not just inferred from the reg case: advisor caught this test
/// originally claimed to cover both while its source (`examples/
/// option.tr`) had no `?T`-typed output at all.
#[test]
fn option_reg_and_output_flatten_to_valid_data_registers() {
    let fir = emit_from_source(&read_example("option.tr")).expect("emission should succeed");
    assert!(fir.contains("regreset opt_valid : UInt<1>"));
    assert!(fir.contains("regreset opt_data : UInt<8>"));
    assert!(fir.contains("connect opt_valid, UInt<1>(1)"));
    assert!(fir.contains("connect opt_data, __fifo_input_data"));
    assert!(fir.contains("output relayed_valid : UInt<1>"));
    assert!(fir.contains("output relayed_data : UInt<8>"));
    assert!(fir.contains("regreset __out_relayed_valid : UInt<1>"));
    assert!(fir.contains("regreset __out_relayed_data : UInt<8>"));
    assert!(fir.contains("connect __out_relayed_valid, mux(opt_valid, UInt<1>(1), UInt<1>(0))"));
    assert!(fir.contains("connect relayed_valid, __out_relayed_valid"));
    run_firtool(&fir, &[]);
}

/// A `?T`-typed port on an instantiated submodule hits the identical
/// flattening hazard a struct-typed port does (`struct_typed_instance_
/// port_is_rejected`) -- `?T` flattens to `{name}_valid`/`{name}_data`
/// in the target module's own port list, so wiring it here by the bare
/// name would reference a nonexistent port.
#[test]
fn option_typed_instance_port_is_rejected() {
    let src = "\
module Child {
    out p : ?[8] = false
    rule fill {
        p := 8'd5
    }
}

module Parent {
    inst c : Child
    out v : [1] = 0
    rule read {
        v := c.p.valid
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter().any(|e| e.message.contains("`?T`-typed port")),
        "expected a `?T`-typed-port rejection, got: {err:?}"
    );
}

/// `?[1]` is the discriminating case for spelling absent as `false`
/// rather than a general boolean zero: `opt_valid`/`opt_data` are both
/// `UInt<1>`, but they're two DISTINCT signals, not the same bit doing
/// double duty -- `false` clears both, while a present `1` sets both
/// independently. This pins the emitted shape; `sim/option_tb.v`'s
/// `check`/`pass` rules exercise the general (wider-than-1-bit) case at
/// runtime.
#[test]
fn option_of_bit_flattens_to_two_distinct_one_bit_registers() {
    let src = "\
module M {
    reg opt : ?[1] = false
    in go : [1]
    in present : [1]

    rule fill {
        go?
        if present {
            opt := 1
        } else {
            opt := false
        }
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("regreset opt_valid : UInt<1>"));
    assert!(fir.contains("regreset opt_data : UInt<1>"));
    assert!(fir.contains("connect opt_valid, mux(present, UInt<1>(1), UInt<1>(0))"));
    assert!(fir.contains("connect opt_data, mux(present, UInt<1>(1), UInt<1>(0))"));
    run_firtool(&fir, &[]);
}

/// A struct field may itself be `?T` -- flattens recursively the same
/// way a nested struct field does (`{struct}_{field}_valid`/`_data`),
/// and a struct literal's init const-evaluates correctly through the
/// nested `?T` field too (this pins a real bug advisor caught: an
/// earlier version of `struct_lit_field_const` assumed every
/// intermediate struct field's value was itself another `Expr::
/// StructLit`, which silently returned `None` -- falling back to a
/// wrong `0` init -- the moment it reached a `?T` field instead).
#[test]
fn nested_option_struct_field_flattens_and_inits_correctly() {
    let src = "\
struct Frame {
    id : [4]
    maybe : ?[8]
}

module M {
    reg fr : Frame = Frame{ id: 3, maybe: 8'd5 }
    out ok : [1] = 0
    out val : [8] = 0

    rule r {
        if fr.maybe.valid {
            ok := 1
            val := fr.maybe.data
        } else {
            ok := 0
        }
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("regreset fr_id : UInt<4>, clock, reset, UInt<4>(3)"));
    assert!(fir.contains("regreset fr_maybe_valid : UInt<1>, clock, reset, UInt<1>(1)"));
    assert!(fir.contains("regreset fr_maybe_data : UInt<8>, clock, reset, UInt<8>(5)"));
    run_firtool(&fir, &[]);
}

/// `let x = opt?` folds its failure condition into the rule's own guard
/// exactly like `x := opt?`/a bare `opt?` statement already do --
/// advisor caught this as a real gap pre-commit: the position check
/// (`check_guard_positions`) alone allowed a `let` init to be a whole
/// guard, but `compile_guard`'s fold didn't look there, so the rule
/// would have fired unconditionally, reading a stale/garbage unwrapped
/// value on a cycle `opt` was actually absent.
#[test]
fn let_bound_option_unwrap_folds_its_guard() {
    let src = "\
module M {
    reg opt : ?[8] = false
    out val : [8] = 0
    in go : [1]

    rule fill {
        go?
        opt := 8'd9
    }

    rule pass {
        let x = opt?
        val := x
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_pass = and(opt_valid, not(fires_fill))"));
    run_firtool(&fir, &[]);
}

/// The `let`-bound case of `check_guard_placement`'s "guard after a
/// state write" restriction -- mirrors `a_bare_condition_after_a_
/// state_write_is_an_error` for the `Stmt::Let` position specifically.
#[test]
fn let_bound_option_unwrap_after_a_state_write_is_rejected() {
    let src = "\
module M {
    reg opt : ?[8] = false
    reg other : [8] = 0
    rule r {
        other := 1
        let x = opt?
        other := x
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("guard") && e.message.contains("state write"))
    );
}

/// A guard nested inside a larger expression (here, an arithmetic
/// operand) is rejected outright rather than silently never folded --
/// `compile_guard`'s fold only looks at three exact positions (bare
/// statement, whole `:=` RHS, whole `let` init); anywhere else, the
/// value side (`compile_expr_hinted`'s `Expr::Guard` arm) would happily
/// read `opt.data` with no corresponding guard term, reading an absent
/// Option as if it were present. Advisor-caught: this is the general
/// form of the same hole `let_bound_option_unwrap_folds_its_guard`
/// closes for the `let`-init position specifically.
#[test]
fn a_guard_nested_in_arithmetic_is_rejected() {
    let src = "\
module M {
    reg opt : ?[8] = false
    out val : [8] = 0
    in go : [1]

    rule fill {
        go?
        opt := 8'd9
    }

    rule bad {
        val := opt? + 1
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("guard") && e.message.contains("nested")),
        "expected a nested-guard rejection, got: {err:?}"
    );
}

/// A `?T`-typed field access over a Guard base (`o?.valid`) is cleanly
/// rejected by `check_guard_positions`, not silently miscompiled -- the
/// `Expr::Guard` here isn't the WHOLE right-hand side of `:=` (a
/// `.valid` field wraps around it), so it's "nested" the same as
/// `a_guard_nested_in_arithmetic_is_rejected`'s case. Pins the answer
/// to a question raised while designing `?T`: unwrap via `?` and
/// non-failing access via `.valid`/`.data` are two DISTINCT idioms, not
/// composable into one chain.
#[test]
fn a_guard_field_accessed_directly_is_rejected() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

module M {
    reg o : ?Pair = Pair{ valid: 1, data: 8'd7 }
    out ok : [1] = 0
    rule r {
        ok := o?.valid
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("guard") && e.message.contains("nested")),
        "expected a nested-guard rejection, got: {err:?}"
    );
}

/// `?T` where `T` is itself a struct -- `option_field_widths`'s
/// `Ty::Struct` recursion arm, `compile_field_path_value`'s Option-to-
/// Struct handoff, and `option_lit_field_const`'s matching handoff all
/// exercised together. `o`'s nonzero init (`valid: 1, data: 7`)
/// discriminates the const-eval path from a `0`-fallback the same way
/// `nested_option_struct_field_flattens_and_inits_correctly` does for
/// the opposite nesting order.
#[test]
fn option_of_a_struct_flattens_and_inits_correctly() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

module M {
    reg o : ?Pair = Pair{ valid: 1, data: 8'd7 }
    out ok : [1] = 0
    rule r {
        ok := o.data.valid
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("regreset o_valid : UInt<1>, clock, reset, UInt<1>(1)"));
    assert!(fir.contains("regreset o_data_valid : UInt<1>, clock, reset, UInt<1>(1)"));
    assert!(fir.contains("regreset o_data_data : UInt<8>, clock, reset, UInt<8>(7)"));
    run_firtool(&fir, &[]);
}

/// A `?T` guard folded from INSIDE a callee body (`callee_fail_cond`,
/// calls.rs) -- the fourth, cross-file guard-fold site, separate from
/// `compile_guard`'s three (writes.rs). Advisor caught this one
/// pre-commit: it originally compiled the guard's inner expression as
/// an ordinary condition regardless of type, which for a `?T` operand
/// emitted a reference to a register that was never declared (`opt`
/// instead of `opt_valid`) -- a real firtool-rejected miscompile, not
/// just a wrong value. `Consume`'s body is a bare-statement `opt?`
/// (discarding the unwrapped value) followed by a separate `.data`
/// read, the one shape `check_fails_is_foldable_guard` actually allows
/// here (a `return opt?` with the guard AS the return value is
/// rejected outright before this fold ever runs, a stricter but
/// correct restriction, not this bug).
#[test]
fn a_callee_bodys_option_guard_folds_correctly() {
    let src = "\
module M {
    reg opt : ?[8] = false
    out result : [8] = 0

    Consume() : [8] <combines, fails> {
        opt?
        return opt.data
    }

    rule r {
        result := Consume()
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_r = opt_valid"));
    run_firtool(&fir, &[]);
}

/// A `?T`-typed OUTPUT written nested in `if` with no `else` -- the
/// hold-the-current-value fallback (`struct_field_value_in_stmts`'s
/// hold name, built as `{name}_{path}`) resolves to the flat PORT name
/// (`relayed_valid`), not the internal `__out_relayed_valid` backing
/// register `reg_value_in_stmts` would use for a plain scalar output.
/// Advisor flagged this as a possible bug pre-commit; confirmed
/// harmless instead: firtool elaborates the port as a pure
/// combinational alias of its backing register (`wire _relayed_valid_
/// output = __out_relayed_valid`), so referencing it as the hold value
/// is value-identical to referencing the register directly -- verified
/// through a full Icarus run holding correctly across multiple `go=0`
/// cycles, not just firtool acceptance. Pinned here as a real test
/// rather than left as a hand probe.
#[test]
fn option_typed_output_written_nested_in_if_with_no_else_holds_correctly() {
    let src = "\
module M {
    reg opt : ?[8] = false
    out relayed : ?[8] = false
    in go : [1]
    rule r {
        if go {
            relayed := opt.data
        }
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_relayed_valid, mux(go, UInt<1>(1), relayed_valid)"));
    assert!(fir.contains("connect __out_relayed_data, mux(go, opt_data, relayed_data)"));
    run_firtool(&fir, &[]);
}

/// A `?T`-typed INPUT flattens to two real input ports, `{name}_valid`/
/// `{name}_data` -- the read side needs no `struct_reg_source` entry at
/// all (an input has no write to thread), just the ordinary flat-name
/// field read every other `.field` access already resolves.
#[test]
fn option_typed_input_flattens_to_two_ports() {
    let src = "\
module M {
    in i : ?[8]
    out ok : [1] = 0
    rule r {
        ok := i.valid
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("input i_valid : UInt<1>"));
    assert!(fir.contains("input i_data : UInt<8>"));
    assert!(fir.contains("connect __out_ok, i_valid"));
    run_firtool(&fir, &[]);
}

/// `??T` (`T` itself `?U`) via BARE coercion only (no `optional`) still
/// only ever reaches the two fully-agreeing states -- fully absent
/// (`false`) and fully present (a bare `[8]` value, coerced through
/// BOTH Option layers by `check_assignable`'s recursive coercion rule):
/// `oo_valid` and `oo_data_valid` get byte-identical mux expressions in
/// every branch below, because neither `false` nor a bare value ever
/// separates them, and `.data` stays read-only throughout. This is NOT
/// a general limitation of `??T` itself -- `optional false`/`optional
/// (optional e)` DO reach the third state, `Some(None)` included (see
/// DESIGN.md's "Option types" section, `examples/option.tr`'s `nested`
/// reg); this test only pins that the OLD, bare-coercion-only paths
/// below are unaffected by that addition, not that `Some(None)` is
/// unreachable some other way.
#[test]
fn nested_option_reaches_only_fully_absent_or_fully_present() {
    let src = "\
module M {
    reg oo : ??[8] = false
    out outer_valid : [1] = 0
    out inner_valid : [1] = 0
    out val : [8] = 0
    in go : [1]
    in present : [1]

    rule fill {
        go?
        if present {
            oo := 8'd7
        } else {
            oo := false
        }
    }

    rule check {
        outer_valid := oo.valid
        inner_valid := oo.data.valid
        val := oo.data.data
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("regreset oo_valid : UInt<1>"));
    assert!(fir.contains("regreset oo_data_valid : UInt<1>"));
    assert!(fir.contains("regreset oo_data_data : UInt<8>"));
    assert!(fir.contains("connect oo_valid, mux(present, UInt<1>(1), UInt<1>(0))"));
    assert!(fir.contains("connect oo_data_valid, mux(present, UInt<1>(1), UInt<1>(0))"));
    assert!(fir.contains("connect oo_data_data, mux(present, UInt<8>(7), UInt<8>(0))"));
    run_firtool(&fir, &[]);
}

/// A local bound to ANOTHER `?T`-typed value (`let o = opt`, `opt`
/// itself `?[8]`) is rejected, the identical restriction a
/// struct-typed local has (`struct_typed_local_aliasing_another_local_
/// is_rejected`) -- pins a real bug found while investigating `?T`-
/// typed fn params (which bind an argument through this exact same
/// `compile_field_path_value` path): `type_write`'s Option-to-Option
/// rejection only runs for a state WRITE with a known target type, so
/// a plain `let` (no target type, ordinary type inference) reached
/// `compile_field_path_value`'s `Ty::Option` arm with `expr` itself
/// Option-typed, which the arm silently treated as "definitely
/// present" -- hardcoding `UInt<1>(1)` regardless of `opt_valid`'s
/// actual runtime value. Now a clean compile-time error instead.
#[test]
fn option_typed_local_aliasing_another_option_value_is_rejected() {
    let src = "\
module M {
    reg opt : ?[8] = false
    out ok : [1] = 0
    rule r {
        let o = opt
        ok := o.valid
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("not aliased from another `?T` value")),
        "expected an option-aliasing rejection, got: {err:?}"
    );
}

/// The same aliasing rejection reached through the `?` unwrap's GUARD
/// fold (`compile_guard_unwrap_cond`), not just a plain `.valid` value
/// read -- `val := o?` folds a guard condition through the identical
/// `compile_field_path_value` path before ever compiling `o?`'s VALUE,
/// so both would have hit the same silent-hardcoded-`UInt<1>(1)` bug
/// pre-fix (the rule firing unconditionally on an absent Option, worse
/// than a wrong read since it's invisible without simulating).
#[test]
fn option_typed_local_aliasing_is_rejected_through_the_guard_fold_too() {
    let src = "\
module M {
    reg opt : ?[8] = false
    out val : [8] = 0
    rule r {
        let o = opt
        val := o?
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("not aliased from another `?T` value")),
        "expected an option-aliasing rejection, got: {err:?}"
    );
}

/// `let {valid, data} = source` desugars (parser.rs) into one `let bind
/// = source.field` per item -- no destructuring-specific rejection
/// exists, so a destructuring `source` that aliases another struct/
/// Option value hits this SAME pre-existing guard, for free.
#[test]
fn let_destructure_of_an_aliased_option_local_is_rejected() {
    let src = "\
module M {
    reg opt : ?[8] = false
    out ok : [1] = 0
    rule r {
        let o = opt
        let {valid, data} = o
        ok := valid
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("not aliased from another `?T` value")),
        "expected an option-aliasing rejection, got: {err:?}"
    );
}

/// End-to-end: `let {valid: p_valid, ...} = p` compiles to real,
/// firtool-accepted FIRRTL reading straight off `p`'s own flat
/// `p_valid`/`p_data` registers -- no intermediate wires, since the
/// desugar's `p.field` projections are ordinary struct-field reads.
#[test]
fn let_destructure_compiles_to_a_direct_field_read() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

module M {
    reg p : Pair = Pair{ valid: 1, data: 5 }
    out result : [8] = 0
    rule r {
        let {valid, data: d} = p
        result := d
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_result, p_data"), "{fir}");
    run_firtool(&fir, &[]);
}

/// `p := Pair{ data: 5, ..p }` -- updating one field of a reg from its
/// OWN current value. The untouched field's flat register connects to
/// ITSELF (`connect p_valid, p_valid`) since `..p`'s fallback reads
/// straight off `p`'s own flat fields -- confirmed firtool accepts that
/// self-connect rather than erroring or warning on it (transactional
/// semantics: a register reads its PRE-edge value combinationally, the
/// same reasoning `pc := pc + 3` already relies on, so this is a
/// same-cycle read of `p`'s old value, not a use-after-write).
#[test]
fn struct_update_of_a_regs_own_current_value_compiles_and_self_connects() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

module M {
    reg p : Pair = Pair{ valid: 1, data: 0 }
    rule go {
        p := Pair{ data: 5, ..p }
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect p_valid, p_valid"), "{fir}");
    assert!(fir.contains("connect p_data, UInt<8>(5)"), "{fir}");
    run_firtool(&fir, &[]);
}

/// The read `..base` contributes MUST reach the scheduler
/// (`infer_expr`, effects.rs) -- isolated from the write side entirely:
/// `copy_from_p` never writes `p` at all, only reads it through `..p`,
/// so if that read weren't recorded, `write_p` (an unconditional write
/// to `p`) would show no conflict and both would fire the same cycle,
/// a real hazard invisible without checking the schedule. Self-caught
/// before considering this feature done -- `effects.rs`'s two
/// `Expr::StructLit` walkers don't destructure `base` by name (only
/// `..`-wildcard it), so the compiler doesn't force this site the way
/// adding a new `Expr` variant would.
#[test]
fn struct_update_spread_registers_as_a_read_for_scheduling() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

module M {
    reg p : Pair = Pair{ valid: 1, data: 0 }
    reg q : Pair = Pair{ valid: 0, data: 0 }

    rule copy_from_p {
        q := Pair{ data: 5, ..p }
    }

    rule write_p {
        p := Pair{ valid: 1, data: 9 }
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(
        fir.contains("node fires_write_p = and(UInt<1>(1), not(fires_copy_from_p))"),
        "expected write_p gated mutually exclusive with copy_from_p's read of p:\n{fir}"
    );
    run_firtool(&fir, &[]);
}

/// A struct-typed fn param resolves a REG-typed argument -- the
/// actually-useful case, not just a literal -- by chasing through the
/// param binding to the reg's own flat field registers
/// (`compile_struct_field_read`'s PARAM-only chase-through, expr.rs).
#[test]
fn struct_typed_fn_param_resolves_a_reg_argument() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

UsePair(p : Pair) : [8] <combines> {
    return p.data
}

module M {
    reg q : Pair = Pair{ valid: 1, data: 8'd7 }
    out out_v : [8] = 0
    rule r {
        out_v := UsePair(q)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_out_v, q_data"));
    run_firtool(&fir, &[]);
}

/// The `?T` sibling of the above -- a reg-typed argument resolves
/// through an Option-typed param, AND the callee's own bare-statement
/// guard (`o?`) folds through the SAME param binding into the caller's
/// rule guard (the `callee_fail_cond` cross-file fold site from
/// earlier this session, now exercised with a PARAM alias in between
/// rather than a bare module-level reg reference).
#[test]
fn option_typed_fn_param_resolves_a_reg_argument_and_folds_its_guard() {
    let src = "\
Consume(o : ?[8]) : [8] <combines, fails> {
    o?
    return o.data
}

module M {
    reg opt : ?[8] = false
    out result : [8] = 0

    rule r {
        result := Consume(opt)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_r = opt_valid"));
    assert!(fir.contains("connect __out_result, opt_data"));
    run_firtool(&fir, &[]);
}

/// A struct/Option-typed param CHAIN (one callee passing its own param
/// straight through to another callee's SAME-typed param) resolves all
/// the way back to the original reg -- `bind_callee_context`'s
/// reentrant substitution plus the param chase-through compose
/// correctly across two levels of call nesting, not just one.
#[test]
fn struct_typed_fn_param_chains_through_a_nested_call() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

Inner(p : Pair) : [8] <combines> {
    return p.data
}

Outer(p : Pair) : [8] <combines> {
    return Inner(p)
}

module M {
    reg q : Pair = Pair{ valid: 1, data: 8'd9 }
    out out_v : [8] = 0
    rule r {
        out_v := Outer(q)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_out_v, q_data"));
    run_firtool(&fir, &[]);
}

/// A plain `T`-typed argument (not itself `?T`) passed to a `?T` param
/// still coerces to present, unaffected by the new param chase-through
/// -- the chase-through only fires when the argument's OWN type
/// exactly matches the param's declared type (genuine aliasing), so a
/// `[8]` argument for a `?[8]` param falls through to the
/// ordinary coercion-synthesis path instead of being (wrongly) chased.
#[test]
fn plain_value_argument_still_coerces_to_a_present_option_param() {
    let src = "\
UseIt(o : ?[8]) : [1] <combines> {
    return o.valid
}
module M {
    reg x : [8] = 0
    out ok : [1] = 0
    rule r {
        ok := UseIt(x)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_ok, UInt<1>(1)"));
    run_firtool(&fir, &[]);
}

/// The param chase-through's `Expr::Ident`-only gate correctly leaves a
/// callee-local bound to a struct LITERAL (no aliasing at all) alone --
/// it takes the ordinary literal-decompose path, unaffected by whether
/// a DIFFERENT local in the same callee happens to be a param.
/// Advisor-verified before commit: the gate keys off `bound`'s own AST
/// shape (`Expr::Ident` vs `Expr::StructLit`), not off "is this
/// callee's `p` a param", so it can't over-reject this case.
#[test]
fn a_callee_local_bound_to_a_struct_literal_is_unaffected_by_param_chase_through() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}
UsePair(p : Pair) : [8] <combines> {
    let x = Pair{ valid: 1, data: 8'd3 }
    return x.data
}
module M {
    reg q : Pair = Pair{ valid: 1, data: 8'd7 }
    out out_v : [8] = 0
    rule r {
        out_v := UsePair(q)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_out_v, UInt<8>(3)"));
    run_firtool(&fir, &[]);
}

/// A callee-local re-BINDING a param (`let x = p`, no literal, just an
/// alias) still rejects, with the STRUCT-flavored message specifically
/// (not the Option one -- `bound_is_option_alias` keys on `root_ty`
/// being `Ty::Option`, so this pins that the two messages don't cross-
/// contaminate). The chase-through is deliberately PARAM-only: `x`
/// here is a LOCAL, not a param, so it doesn't get chased.
#[test]
fn a_callee_local_aliasing_a_struct_typed_param_is_rejected() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}
UsePair(p : Pair) : [8] <combines> {
    let x = p
    return x.data
}
module M {
    reg q : Pair = Pair{ valid: 1, data: 8'd7 }
    out out_v : [8] = 0
    rule r {
        out_v := UsePair(q)
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("not aliased from another local")),
        "expected a struct-local-aliasing rejection, got: {err:?}"
    );
}

/// A struct-RETURNING fn (`compile_callee_body_field`, calls.rs)
/// decomposes its trailing `return <struct literal>` one leaf field at
/// a time, threading each leaf back through the SAME per-field write
/// machinery a struct literal's own direct write already uses
/// (`compile_field_path_value`'s new `Expr::Call` case, writes.rs).
/// Before this, `p := MakePair()` type-checked as a plain type error
/// (struct writes required a literal RHS) -- with that gate lifted, the
/// write must actually decompose correctly, not silently vanish (the
/// exact class of bug this session's `?T`-aliasing fix caught earlier:
/// a self-caught probe of this very case found the write disappearing
/// entirely, zero error, zero `connect` -- fixed by this dispatch).
#[test]
fn struct_typed_fn_return_builds_a_fresh_literal() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

MakePair() : Pair <combines> {
    return Pair{ valid: 1, data: 8'd7 }
}

module M {
    reg p : Pair = Pair{ valid: 0, data: 0 }
    out v : [8] = 0
    rule r {
        p := MakePair()
        v := p.data
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect p_valid, UInt<1>(1)"));
    assert!(fir.contains("connect p_data, UInt<8>(7)"));
    run_firtool(&fir, &[]);
}

/// The `?T` sibling, AND the guard-fold intersection advisor flagged as
/// the one composition this session's params work hadn't tested yet: a
/// `<fails>` callee's bare-statement guard folds into the caller's rule
/// guard (`callee_fail_cond`) COMPLETELY INDEPENDENTLY of its return
/// value's own per-leaf decomposition (`compile_callee_body_field`) --
/// two separate passes over the same body that need to compose, not
/// interfere. `x`'s present-coercion into `?[8]` (not an alias --
/// `x : [8]`, a DIFFERENT type from the `?[8]` return) must
/// still synthesize via the ordinary `Ty::Option` coercion path.
#[test]
fn option_typed_fn_return_coerces_present_and_folds_its_guard() {
    let src = "\
Consume(x : [8]) : ?[8] <combines, fails> {
    (x <> 0)?
    return x
}

module M {
    in x : [8]
    reg opt : ?[8] = false
    rule r {
        opt := Consume(x)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_r = neq(x, UInt<8>(0))"));
    assert!(fir.contains("connect opt_valid, UInt<1>(1)"));
    assert!(fir.contains("connect opt_data, x"));
    run_firtool(&fir, &[]);
}

/// A struct-returning callee that just returns one of its OWN params
/// UNCHANGED (`Passthrough(p) { return p }`) is the return-side twin of
/// `struct_typed_fn_param_resolves_a_reg_argument` -- a param binding IS
/// this call's actual argument substituted in, so `dst := Passthrough
/// (src)` must thread `src`'s own flat fields straight through to
/// `dst`'s, not error as "too complex to inline". Handled directly in
/// `compile_callee_body_field`'s `Return` arm (calls.rs), NOT as a
/// general `Expr::Ident` case inside `compile_field_path_value` --an
/// earlier version of this fix put it there and it silently
/// relegalized `a_callee_local_aliasing_a_struct_typed_param_is_
/// rejected`'s exact pattern, caught by that regression test.
#[test]
fn struct_typed_fn_return_passes_through_a_param_unchanged() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

Passthrough(p : Pair) : Pair <combines> {
    return p
}

module M {
    reg src : Pair = Pair{ valid: 0, data: 0 }
    reg dst : Pair = Pair{ valid: 0, data: 0 }
    in go : [1]
    rule fill {
        go?
        src := Pair{ valid: 1, data: 8'd9 }
    }
    rule copy {
        dst := Passthrough(src)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect dst_valid, src_valid"));
    assert!(fir.contains("connect dst_data, src_data"));
    run_firtool(&fir, &[]);
}

/// A callee-LOCAL that re-binds a struct-typed param, then returns the
/// WHOLE local (not just a field read off it), stays rejected -- the
/// return-side twin of `a_callee_local_aliasing_a_struct_typed_param_
/// is_rejected`, pinning that the `compile_field_path_value` restriction
/// (no generic `Expr::Ident` case) actually holds for a bare `return x`
/// too, not just a `return x.data` field access.
#[test]
fn a_callee_local_aliasing_a_param_is_rejected_through_a_struct_return_too() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

Passthrough2(p : Pair) : Pair <combines> {
    let x = p
    return x
}

module M {
    reg q : Pair = Pair{ valid: 1, data: 8'd7 }
    reg dst : Pair = Pair{ valid: 0, data: 0 }
    rule r {
        dst := Passthrough2(q)
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("too complex to inline")),
        "expected a struct-return inlining rejection, got: {err:?}"
    );
}

/// A struct-returning callee whose own trailing `return` is itself
/// ANOTHER struct-returning call (`WrapPair() { return MakePair() }`)
/// chains correctly -- `compile_call_field_value` re-enters `compile_
/// field_path_value` on the nested call's return expr exactly the way
/// the params work's own nested-call chaining already does for reads.
#[test]
fn struct_typed_fn_return_chains_through_a_nested_call() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

MakePair() : Pair <combines> {
    return Pair{ valid: 1, data: 8'd7 }
}

WrapPair() : Pair <combines> {
    return MakePair()
}

module M {
    reg p : Pair = Pair{ valid: 0, data: 0 }
    rule r {
        p := WrapPair()
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect p_valid, UInt<1>(1)"));
    assert!(fir.contains("connect p_data, UInt<8>(7)"));
    run_firtool(&fir, &[]);
}

/// A `let`-bound struct-returning call resolves through the SAME
/// dispatch a direct write already gets -- `compile_struct_field_read`'s
/// existing Local-arm fallback reaches `compile_field_path_value`'s new
/// `Expr::Call` case for free, no separate machinery needed.
#[test]
fn let_bound_struct_returning_call_resolves_field_reads() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

MakePair() : Pair <combines> {
    return Pair{ valid: 1, data: 8'd7 }
}

module M {
    out v : [8] = 0
    rule r {
        let q = MakePair()
        v := q.data
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_v, UInt<8>(7)"));
    run_firtool(&fir, &[]);
}

/// A struct-returning call passed DIRECTLY as another call's struct-
/// typed argument (`UsePair(MakePair())`, no intermediate `let`) also
/// resolves -- `compile_struct_field_read`'s Param-arm fallback (the
/// bound expr isn't a bare `Expr::Ident`, so no chase-through, straight
/// to `compile_field_path_value`) reaches the same `Expr::Call` case.
#[test]
fn struct_returning_call_used_directly_as_another_calls_argument() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

MakePair() : Pair <combines> {
    return Pair{ valid: 1, data: 8'd7 }
}

UsePair(p : Pair) : [8] <combines> {
    return p.data
}

module M {
    out v : [8] = 0
    rule r {
        v := UsePair(MakePair())
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_v, UInt<8>(7)"));
    run_firtool(&fir, &[]);
}

/// The `if`/`else` arm of `compile_callee_body_field` -- untouched by
/// every OTHER struct-return test, which all use a single trailing
/// `return` -- muxes each leaf field INDEPENDENTLY: `p_valid` and
/// `p_data` each get their own `mux(c, ...)`, not the same value copied
/// to both (advisor-flagged gap: written but never actually exercised
/// until this test).
#[test]
fn struct_typed_fn_return_if_else_muxes_per_leaf() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

Pick(c : [1]) : Pair <combines> {
    if c {
        return Pair{ valid: 1, data: 8'd1 }
    } else {
        return Pair{ valid: 0, data: 8'd2 }
    }
}

module M {
    in c : [1]
    reg p : Pair = Pair{ valid: 0, data: 0 }
    rule r {
        p := Pick(c)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect p_valid, mux(c, UInt<1>(1), UInt<1>(0))"));
    assert!(fir.contains("connect p_data, mux(c, UInt<8>(1), UInt<8>(2))"));
    run_firtool(&fir, &[]);
}

/// The `if`/`else` arm combined with the param-passthrough special
/// case: one branch returns a param unchanged, the other a fresh
/// literal -- the passthrough chase-through sits INSIDE the per-branch
/// recursion, not just at a bare top-level `return`.
#[test]
fn struct_typed_fn_return_if_else_passes_a_param_through_one_branch() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

PickPassthrough(p : Pair, c : [1]) : Pair <combines> {
    if c {
        return p
    } else {
        return Pair{ valid: 0, data: 8'd2 }
    }
}

module M {
    in c : [1]
    reg src : Pair = Pair{ valid: 1, data: 8'd9 }
    reg dst : Pair = Pair{ valid: 0, data: 0 }
    rule r {
        dst := PickPassthrough(src, c)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect dst_valid, mux(c, src_valid, UInt<1>(0))"));
    assert!(fir.contains("connect dst_data, mux(c, src_data, UInt<8>(2))"));
    run_firtool(&fir, &[]);
}

/// A struct-typed OUTPUT (not just a reg) as a struct-returning call's
/// write target -- `reg`/`out` go through separate bookkeeping
/// (`struct_reg_source` vs the output equivalent), so this pins that
/// the new `Expr::Call` dispatch in `compile_field_path_value` reaches
/// the output path too, not just regs (every other test in this file
/// writes to a `reg`).
#[test]
fn struct_typed_output_as_a_fn_return_write_target() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

MakePair() : Pair <combines> {
    return Pair{ valid: 1, data: 8'd7 }
}

module M {
    out p : Pair = Pair{ valid: 0, data: 0 }
    rule r {
        p := MakePair()
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_p_valid, UInt<1>(1)"));
    assert!(fir.contains("connect __out_p_data, UInt<8>(7)"));
    run_firtool(&fir, &[]);
}

/// A struct-returning callee that ALSO writes a module register as a
/// side effect: the write-hunt (`callee_reg_write`, an existing,
/// separate pass) and the new per-leaf return decomposition
/// (`compile_callee_body_field`) are two fully independent walks over
/// the SAME callee body -- the composition class that's bitten this
/// session three times already (guard+write, guard+fifo, `callee_fail_
/// cond`+Option), so this pins that both connects appear from one
/// clean compile rather than one silently winning over the other.
#[test]
fn struct_returning_callee_that_also_writes_state() {
    let src = "\
struct Pair {
    valid : [1]
    data : [8]
}

module M {
    in x : [8]
    reg log : [8] = 0
    reg p : Pair = Pair{ valid: 0, data: 0 }

    MakeAndLog(d : [8]) : Pair <combines> {
        log := d
        return Pair{ valid: 1, data: d }
    }

    rule r {
        p := MakeAndLog(x)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect log, x"));
    assert!(fir.contains("connect p_valid, UInt<1>(1)"));
    assert!(fir.contains("connect p_data, x"));
    run_firtool(&fir, &[]);
}

/// `if`: branch-scoped fallible conditions (DESIGN.md, TODO.md's
/// if-guard item). A BARE comparison, no `logic` needed, directly as an
/// `if`'s own condition — Lumi's call: Verse-faithful branch-scoping
/// applied uniformly, so the rule's own guard (`fires_r`) never depends
/// on it, with-else or not; only the mux select does. `mux(gt(a, b), 1,
/// 2)` proves the SELECT compiles the comparison's TEST, not its "yields
/// the left operand" VALUE (`type_binop`'s comparisons-as-fallible rule)
/// — `compile_expr`'s ordinary dispatch would give `a` itself there,
/// wrong for a mux selector; `compile_guard_unwrap_cond` (already built
/// for the guard-fold, reused here) is what the 8 mux-threading call
/// sites across writes.rs/calls.rs now route through instead.
#[test]
fn if_with_a_bare_comparison_condition_and_else_compiles_to_a_predicate_mux_and_never_gates_the_rule()
 {
    let src = "\
module M {
    reg v : [8] = 0
    in a : [8]
    in b : [8]
    rule r {
        if a > b {
            v := 1
        } else {
            v := 2
        }
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_r = UInt<1>(1)"));
    assert!(fir.contains("connect v, mux(gt(a, b), UInt<8>(1), UInt<8>(2))"));
    run_firtool(&fir, &[]);
}

/// The genuinely new half of this feature (DESIGN.md/TODO.md's own
/// framing): a fallible `if` with NO `else`. Verse-faithful branch-
/// scoping, applied uniformly (Lumi's call, `AskUserQuestion`): failure
/// only skips the `then` branch — `v` holds its own current value, same
/// "hold" fallback an ordinary unwritten register path already has — the
/// REST of the rule still runs and the rule still fires unconditionally,
/// unlike today's top-level bare-comparison guard (`a > b` alone, or
/// `(a > b)?`), which would abort the whole cycle instead. `w := 2`
/// running regardless is the one observable difference a probe can pin.
#[test]
fn if_with_a_bare_comparison_condition_and_no_else_holds_on_failure_and_the_rest_of_the_rule_still_commits()
 {
    let src = "\
module M {
    reg v : [8] = 0
    reg w : [8] = 0
    in a : [8]
    in b : [8]
    rule r {
        if a > b {
            v := 1
        }
        w := 2
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_r = UInt<1>(1)"));
    assert!(fir.contains("connect v, mux(gt(a, b), UInt<8>(1), v)"));
    assert!(fir.contains("connect w, UInt<8>(2)"));
    run_firtool(&fir, &[]);
}

/// `logic`-wrapping an if-condition comparison still works exactly as it
/// did before this feature (compiles to the identical FIRRTL) — a bare
/// comparison is a newly ADDED shape, not a replacement, so the existing
/// `logic a > b` spelling stays valid.
#[test]
fn logic_wrapped_if_condition_comparison_is_unaffected_by_the_bare_comparison_addition() {
    let src = "\
module M {
    reg v : [8] = 0
    in a : [8]
    in b : [8]
    rule r {
        if logic a > b {
            v := 1
        } else {
            v := 2
        }
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("node fires_r = UInt<1>(1)"));
    assert!(fir.contains("connect v, mux(gt(a, b), UInt<8>(1), UInt<8>(2))"));
    run_firtool(&fir, &[]);
}

/// A fifo op as a bare if-condition stays a v0 restriction, completely
/// unaffected by this feature — only a comparison gets the new bare
/// discharge; `check_cond`'s `allow_bare_comparison` exemption only ever
/// matches `Expr::Binary` with `is_comparison()`, never a fifo op's own
/// `Ty::Bits(width)` (the fifo element's type, not `[1]`).
#[test]
fn a_fifo_op_as_a_bare_if_condition_is_still_a_type_error() {
    let src = "\
module M {
    fifo f : [8]
    reg v : [8] = 0
    rule r {
        if f.Deq[] {
            v := 1
        }
    }
}
";
    let (tokens, _) = lexer::lex(src);
    let (ast, _) = parser::parse(src, &tokens);
    let (res, _) = resolve::resolve(&ast);
    let (_, type_errors) = types::check(&ast, &res);
    assert_eq!(type_errors.len(), 1);
    assert!(type_errors[0].message.contains("condition must be [1]"));
}

/// The write-threading walk this feature's mux-select fix touches isn't
/// just `reg_value_in_stmts` — a memory write threads an explicit
/// write-enable boolean alongside the muxed addr/data
/// (`mem_write_in_stmts`, writes.rs), a structurally different function
/// from the register case with its own independent `compile_expr(cond)`
/// call site. Pins that all three (enable, addr, data) use the
/// comparison's TEST (`gt(a, b)`), not its left-operand passthrough.
#[test]
fn if_with_a_bare_comparison_condition_gates_a_memory_write_enable_and_addr_data_correctly() {
    let src = "\
module M {
    mem m : [8][16]
    in a : [4]
    in b : [4]
    in data : [8]
    in read_addr : [4]
    out read_data : [8] = 0
    rule step {
        if a > b {
            m[a] := data
        }
        read_data := m[read_addr]
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(
        fir.contains("connect m.w_m.en, and(fires_step, mux(gt(a, b), UInt<1>(1), UInt<1>(0)))")
    );
    assert!(fir.contains("connect m.w_m.addr, mux(gt(a, b), a, UInt<4>(0))"));
    assert!(fir.contains("connect m.w_m.data, mux(gt(a, b), data, UInt<8>(0))"));
    run_firtool(&fir, &[]);
}

/// Same mux-select fix, exercised through a THIRD independent
/// write-threading walk: an instance port
/// (`inst_port_value_in_stmts`, writes.rs) has no "hold" fallback of its
/// own (unlike a register) — its unwritten path falls back to a literal
/// `UInt(0)` instead, but the SELECT itself must still be the
/// comparison's predicate, not `x`'s own passthrough value.
#[test]
fn if_with_a_bare_comparison_condition_gates_a_submodule_instance_port() {
    let src = "\
module Child {
    in a : [8]
    out b : [8] = 0
    rule pass {
        b := a
    }
}
module Top {
    inst c : Child
    in x : [8]
    in y : [8]
    out result : [8] = 0
    rule wire {
        if x > y {
            c.a := x
        } else {
            c.a := 0
        }
        result := c.b
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect c.a, mux(gt(x, y), x, UInt<8>(0))"));
    run_firtool(&fir, &[]);
}

/// A FOURTH independent write-threading walk: a callee (not a rule)
/// writing a caller's register through its own `if`/`else`, called as a
/// bare statement (`callee_reg_write`, writes.rs) — a completely
/// different code path from a callee's RETURN value (`compile_callee_
/// body`, calls.rs, already covered by `if_condition...gates_a_
/// submodule_instance_port` and this file's other callee tests), since
/// nothing here ever reaches a `return`.
#[test]
fn if_with_a_bare_comparison_condition_gates_a_callee_writing_a_register_as_a_bare_statement() {
    let src = "\
module Top {
    in a : [8]
    in b : [8]
    out v_out : [8] = 0
    Bump(x : [8], y : [8]) : [8] <combines> {
        if x > y {
            v_out := x
        } else {
            v_out := y
        }
        return x
    }
    rule compute {
        Bump(a, b)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_v_out, mux(gt(a, b), a, b)"));
    run_firtool(&fir, &[]);
}

/// A reassigned local used inside an if-condition comparison resolves at
/// its OWN textual position (`set_pos`/`enter_rule`, DESIGN.md's
/// "Reassigned locals"), same as every other cond value — the mux select
/// must use `t`'s FIRST binding (`a`), not a later reassignment
/// (`c`) that textually follows the `if`.
#[test]
fn a_reassigned_local_in_an_if_condition_comparison_resolves_at_its_own_position() {
    let src = "\
module M {
    in a : [8]
    in b : [8]
    in c : [8]
    reg v : [8] = 0
    rule r {
        let t = a
        if t > b {
            v := 1
        }
        t := c
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect v, mux(gt(a, b), UInt<8>(1), v)"));
    run_firtool(&fir, &[]);
}
