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
    reg x : bits[1] = 0
    rule r <sequences> {
        if x == 1 {
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
fn fifo_enq_and_deq_same_cycle_is_an_error() {
    let src = "\
module M {
    fifo f : bits[8]
    rule r {
        x := f.Deq[]
        f.Enq[x]
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(err.iter().any(|e| e.message.contains("same cycle")));
}

#[test]
fn fifo_op_nested_in_if_is_an_error() {
    let src = "\
module M {
    fifo f : bits[8]
    reg cond : bits[1] = 0
    rule r {
        if cond == 1 {
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
    fifo f : bits[8]
    reg x : bits[8] = 0
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

#[test]
fn reassigned_local_is_an_error_not_a_silent_miscompile() {
    // A local read between two assignments must not silently inline
    // the *later* binding: `y` should see `r`, not `r + 1`. Rather than
    // risk that, reassigning a local in an emitted rule is rejected.
    let src = "\
module M {
    fifo f : bits[8]
    reg r : bits[8] = 0
    rule test {
        x := r
        y := x
        x := r + 1
        f.Enq[y]
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(err.iter().any(|e| e.message.contains("reassigned")));
}

#[test]
fn errors_on_nested_mem_write() {
    let src = "\
module M {
    reg cond : bits[1] = 0
    mem m : bits[8][16]
    reg addr : bits[8] = 0
    reg v : bits[8] = 0

    rule r {
        if cond == 1 {
            m[addr] := v
        }
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("memory write nested"))
    );
}

#[test]
fn errors_on_ambiguous_top_module() {
    // Two modules that don't instantiate each other: still an error, just
    // reworded now that multiple modules is legal when one instantiates
    // the other (see `emits_submodule_instance`).
    let src = "\
module A {
    reg x : bits[1] = 0
}
module B {
    reg y : bits[1] = 0
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
fn errors_on_nested_inst_write() {
    let src = "\
module Child {
    input a : bits[8]
    output b : bits[8] = 0
    rule pass {
        b := a
    }
}
module Top {
    inst c : Child
    reg cond : bits[1] = 0
    reg v : bits[8] = 0
    rule r {
        if cond == 1 {
            c.a := v
        }
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("instance port write nested"))
    );
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
    input a : bits[8]
    input b : bits[8]
    output sum : bits[8] = 0
    rule add {
        sum := a + b
    }
}
module Top {
    inst a1 : Adder
    inst a2 : Adder
    input x : bits[8]
    output r1 : bits[8] = 0
    output r2 : bits[8] = 0
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
    input x : bits[8]
    output y : bits[8] = 0
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
fn shift_by_a_non_literal_amount_is_an_error() {
    let src = "\
module M {
    input x : bits[8]
    input n : bits[8]
    output y : bits[8] = 0
    rule r {
        y := x << n
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("shift amount must be a literal"))
    );
}

#[test]
fn bit_select_with_computed_bounds_is_an_error() {
    let src = "\
module M {
    input x : bits[8]
    input i : bits[8]
    output y : bits[1] = 0
    rule r {
        y := x[i]
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("bounds must be literal integers"))
    );
}

#[test]
fn reversed_slice_bounds_are_an_error_not_invalid_firrtl() {
    // types.rs's width formula (`hi.abs_diff(lo) + 1`) accepts either
    // bound order, but FIRRTL's `bits` primop needs hi >= lo — without
    // this check `x[0..3]` would emit `bits(x, 0, 3)`, which firtool
    // rejects with no span back into the .tr source.
    let src = "\
module M {
    input x : bits[8]
    output y : bits[4] = 0
    rule r {
        y := x[0..3]
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(err.iter().any(|e| e.message.contains("hi >= lo")));
}

#[test]
fn logical_not_is_an_error_not_a_silent_bitwise_not() {
    // `!` (UnOp::Not) is deliberately left unimplemented this pass —
    // only `~` (BitNot) and unary `-` (Neg) are supported — so it must
    // error, not silently fall through to some other emission.
    let src = "\
module M {
    input x : bits[8]
    output y : bits[8] = 0
    rule r {
        y := !x
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(err.iter().any(|e| e.message.contains("logical `!`")));
}

#[test]
fn div_and_rem_are_still_errors() {
    let src = "\
module M {
    input x : bits[8]
    input y : bits[8]
    output z : bits[8] = 0
    rule r {
        z := x / y
    }
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("div and rem are not supported"))
    );
}
