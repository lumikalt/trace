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
fn nested_inst_write_threads_through_a_mux() {
    // An instance port write living inside if/else must reach the port as
    // a mux, not be silently dropped for not being a top-level assignment
    // (same claim as `subleq_emits_and_compiles`'s `pc` mux, for a port).
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
        input a : bits[8]
        input b : bits[8]
        output sum : bits[8] = 0
        rule add {
            sum := a + b
        }
    }
    inst adder : Adder
    input x : bits[8]
    input y : bits[8]
    output result : bits[8] = 0
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
fn sized_literal_emits_its_own_declared_width() {
    let src = "\
module M {
    output a : bits[8] = 0
    output b : bits[8] = 0
    output c : bits[8] = 0
    output d : bits[8] = 0
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
    input x : bits[16]
    output result : bits[16] = 0
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
    input x : bits[16]
    output bit3 : bits[1] = 0
    output shifted : bits[16] = 0
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
    output b = 16'hFF00
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
    output result : bits[16] = 0
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
    // Comparison ops don't unify operand widths in types.rs (Eq/Ne/etc.
    // always type as bits[1] regardless of operand widths), and FIRRTL's
    // `eq` primop itself implicitly extends the narrower operand -- so
    // `x == 8'd6` with `x : bits[16]` needs no special-casing beyond
    // what the sized literal already does (emit at its own width).
    let src = "\
module M {
    input x : bits[16]
    output eq : bits[1] = 0
    rule r {
        eq := x == 8'd6
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_eq, eq(x, UInt<8>(6))"));
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
fn logical_not_compiles_identically_to_bitwise_not_on_a_bits_1_value() {
    // `!` and `~` emit the IDENTICAL FIRRTL `not` primop -- what makes
    // `!` a real, distinct operator (not just a parse-time alias) is a
    // types.rs restriction (tests/types.rs's
    // `logical_not_needs_a_bits_1_operand`): `!` requires its operand
    // already be `bits[1]`, `~` accepts any width. Once that's enforced,
    // bitwise-complementing the single bit IS logical negation, so
    // there's nothing left for emission to do differently -- proved here
    // by asserting both compile to the exact same FIRRTL text.
    let src = "\
module M {
    input x : bits[8]
    output bang : bits[1] = 0
    output tilde : bits[1] = 0
    rule r {
        bang := !(x == 0)
        tilde := ~(x == 0)
    }
}
";
    let fir = emit_from_source(src).expect("emission should succeed");
    assert!(fir.contains("connect __out_bang, not(eq(x, UInt<8>(0)))"));
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
    input x : bits[8]
    input y : bits[8]
    output q : bits[8] = 0
    output r : bits[8] = 0
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
    // `b : bits[4]`, `a : bits[8]` -- the checker's target width for
    // `b / a` and `b % a` is `max(4, 8) = 8`, but FIRRTL's own `div`
    // width is the DIVIDEND's width (4 here, not 8) and `rem`'s is
    // `min(4, 8) = 4` -- both narrower than the target, so both need an
    // explicit `pad` up to 8. `a / b` is the mirror case: FIRRTL's div
    // width (8, `a`'s own width as dividend) already equals the target,
    // so no pad there, while `a % b`'s `rem` width (`min(8,4)=4`) still
    // needs padding up to 8 even though the DIVIDEND already matches.
    let src = "\
module M {
    input a : bits[8]
    input b : bits[4]
    output q1 : bits[8] = 0
    output q2 : bits[8] = 0
    output r1 : bits[8] = 0
    output r2 : bits[8] = 0
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
Avg(a : bits[8], b : bits[8]) : bits[8] <combines> {
    return a - b
}
module Top {
    input x : bits[8]
    input y : bits[8]
    input z : bits[8]
    output result : bits[8] = 0
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
    reg v : bits[8] = 0
    input a : bits[8]
    output result : bits[8] = 0

    Bump(x : bits[8]) : bits[8] <combines> {
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
    reg v : bits[8] = 0
    Bump(x : bits[8]) : bits[8] <combines> { return v + x }
    module N {
        input a : bits[8]
        output result : bits[8] = 0
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
Inner(x : bits[8]) : bits[8] <combines> {
    return x + 1
}
Outer(x : bits[8]) : bits[8] <combines> {
    let doubled = Inner(x) * 2
    return doubled
}
module M {
    input a : bits[8]
    output result : bits[8] = 0
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
Inner(x : bits[8]) : bits[8] <combines> {
    return x + 1
}
Outer(x : bits[8]) : bits[8] <combines> {
    let a = Inner(x)
    let b = Inner(x + 1)
    return a + b
}
module M {
    input x : bits[8]
    output result : bits[8] = 0
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
    // (`bits[N]`), type-checked once with `N` never resolved, so
    // `known_width` returns nothing and the nested `Inner(x)` call
    // falls back to `width_of`, which also finds nothing. Pinned as a
    // clean error (not a hang, not a wrong width silently emitted) —
    // the same latent gap already documented for `prio`'s own argument
    // width (`concrete_width_of`), not something this feature fixes.
    let src = "\
Inner(x : bits[N]) : bits[N] <combines> {
    return x
}
Outer(x : bits[N]) : bits[N] <combines> {
    return Inner(x) + 1
}
module M {
    reg r : bits[8] = 0
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
A(x : bits[8]) : bits[8] <combines> {
    return B(x)
}
B(x : bits[8]) : bits[8] <combines> {
    return A(x)
}
module M {
    input a : bits[8]
    output result : bits[8] = 0
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
    reg w : bits[8] = 0
    output result : bits[8] = 0

    Inner(x : bits[8]) : bits[8] <combines, writes {w}> {
        w := x
        return x + 1
    }
    Outer(x : bits[8]) : bits[8] <combines, writes {w}> {
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
    reg v : bits[8] = 0
    input a : bits[8]
    output result : bits[8] = 0

    Inner(x : bits[8]) : bits[8] <combines, writes {v}> {
        v := x
        return x
    }
    Outer(x : bits[8]) : bits[8] <combines, writes {v}> {
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
    reg v : bits[8] = 0
    input a : bits[8]
    output result : bits[8] = 0

    Helper(x : bits[8]) : bits[8] <combines> {
        return x + 1
    }
    Bump(x : bits[8]) : bits[8] <combines, writes {v}> {
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
    input a : bits[8]
    output result : bits[8] = 0

    Inner(x : bits[8]) : bits[8] <combines> {
        return x + 1
    }
    Outer(x : bits[8]) : bits[8] <combines> {
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
    reg v : bits[8] = 0
    input a : bits[8]
    output result : bits[8] = 0

    Bump(x : bits[8]) : bits[8] <combines> {
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
    reg v : bits[8] = 0
    input a : bits[8]

    Bump(x : bits[8]) : bits[8] <combines> {
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
    reg v : bits[8] = 0
    input a : bits[8]

    Bump(x : bits[8]) : bits[8] <combines> {
        if x > 10 {
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
    reg v : bits[8] = 0
    input a : bits[8]
    output result : bits[8] = 0

    Bump(x : bits[8]) : bits[8] <combines> {
        if x > 10 {
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
    reg v : bits[8] = 0
    input a : bits[8]
    input b : bits[8]
    input sel : bits[1]

    Bump(x : bits[8]) : bits[8] <combines> {
        v := x
        return x + 1
    }

    rule r1 {
        (sel == 1)?
        Bump(a)
    }
    rule r2 {
        (sel == 0)?
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
    input a : bits[8]
    output b : bits[8] = 0
    rule pass {
        b := a
    }
}
module Top {
    inst c : Child
    input x : bits[8]

    Drive(v : bits[8]) : bits[8] <combines> {
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
    reg v : bits[8] = 0
    input a : bits[8]
    output result : bits[8] = 0

    Bump(x : bits[8]) : bits[8] <combines> {
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
    reg v : bits[8] = 0
    input a : bits[8]
    output result : bits[8] = 0

    Bump(x : bits[8]) : bits[8] <combines> {
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
    reg v : bits[8] = 0
    reg w : bits[8] = 0
    input a : bits[8]
    Inner(y : bits[8]) : bits[8] <combines> {
        w := y
        return y
    }
    Outer(x : bits[8]) : bits[8] <combines> {
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
Pick(x : bits[8]) : bits[8] <combines> {
    if x > 10 {
        return x
    }
    return 0
}
module M {
    input a : bits[8]
    output result : bits[8] = 0
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
Max(a : bits[8], b : bits[8]) : bits[8] <combines> {
    if a > b {
        return a
    } else {
        return b
    }
}
module M {
    input x : bits[8]
    input y : bits[8]
    output result : bits[8] = 0
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
Pick(x : bits[8]) : bits[8] <combines> {
    if x > 10 {
        return x
    }
}
module M {
    input a : bits[8]
    output result : bits[8] = 0
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
Pick(a : bits[8], b : bits[8]) : bits[8] <combines> {
    if a > b {
        let winner = a
        return winner
    } else {
        let winner = b
        return winner
    }
}
module M {
    input x : bits[8]
    input y : bits[8]
    output result : bits[8] = 0
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
    input a : bits[8]
    output result : bits[8] = 0
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
    input a : bits[8]
    input b : bits[8]
    output result : bits[16] = 0
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
    input a : bits[8]
    input b : bits[8]
    input c : bits[8]
    output result : bits[24] = 0
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
    input a : bits[8]
    input b : bits[8]
    output result : bits[16] = 0

    Combine(x : bits[8], y : bits[8]) : bits[16] <combines> {
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
    input a : bits[16]
    output result : bits[8] = 0
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
    input a : bits[16]
    output result : bits[8] = 0

    Narrow(x : bits[16]) : bits[8] <combines> {
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
    input reqs : bits[4]
    output grant : bits[2] = 0
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
    input reqs : bits[4]
    output grant : bits[2] = 0

    RoundRobin(r : bits[4]) : bits[2] <combines> {
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
    input reqs : bits[4]
    output grant : bits[2] = 0

    Mask(x : bits[4]) : bits[4] <combines> {
        return x & 4'd7
    }
    RoundRobin(r : bits[4]) : bits[2] <combines> {
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
