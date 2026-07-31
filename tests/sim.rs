//! Runs the SUBLEQ milestone all the way to simulated execution: lower
//! -> emit FIRRTL -> firtool -> Verilog -> iverilog/vvp against
//! sim/subleq_tb.v, and checks the testbench's own PASS/FAIL verdict.
//!
//! This is the actual proof the milestone in DESIGN.md asks for: not
//! just that each pass accepts its input, but that the resulting
//! hardware, run against a real SUBLEQ program, computes the right
//! answer (`mem[11] == 8 - 3`) and halts where it should (`pc == 6`,
//! not some address reached by a wrongly-taken branch).
//!
//! Skips (doesn't fail) if firtool/iverilog aren't on PATH, so `cargo
//! test` stays runnable outside the devenv shell that provides them —
//! same convention as tests/firrtl.rs's `run_firtool`.

use std::io::Write;
use std::process::{Command, Stdio};
use trace::{effects, lexer, lower, parser, resolve, schedule, types};

fn tool_available(name: &str) -> bool {
    Command::new(name).arg("--version").output().is_ok()
}

fn generate_firrtl(tr_src: &str) -> String {
    let (tokens, lex_errors) = lexer::lex(tr_src);
    assert!(lex_errors.is_empty(), "{lex_errors:?}");
    let (ast, parse_errors) = parser::parse(tr_src, &tokens);
    assert!(parse_errors.is_empty(), "{parse_errors:?}");
    let (res, resolve_errors) = resolve::resolve(&ast);
    assert!(resolve_errors.is_empty(), "{resolve_errors:?}");
    let (fx, effect_errors) = effects::check(&ast, &res);
    assert!(effect_errors.is_empty(), "{effect_errors:?}");
    let (ty, type_errors) = types::check(&ast, &res);
    assert!(type_errors.is_empty(), "{type_errors:?}");
    let (lowered, lower_errors) = lower::plan(&ast, &res, &fx, &ty);
    assert!(lower_errors.is_empty(), "{lower_errors:?}");
    let lowered_src = lower::render(&ast, tr_src, &lowered);

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

    trace::firrtl::emit(&ast2, &res2, &fx2, &ty2, &sched2)
        .unwrap_or_else(|e| panic!("emission failed: {e:?}"))
}

/// `disable_opt`: a port-less module (SUBLEQ, via hierarchical-path
/// testbenching) needs `--disable-opt`, or firtool DCEs everything since
/// nothing is observable from outside. A module with a real output port
/// (accumulator.tr) does not — that's the whole point of having ports.
fn firrtl_to_verilog(fir: &str, disable_opt: bool) -> String {
    let mut child = Command::new("firtool")
        // Newer firtool no longer sniffs stdin as FIRRTL by default.
        .arg("-format=fir")
        .args(if disable_opt {
            &["--disable-opt"][..]
        } else {
            &[]
        })
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
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "firtool failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Compile `verilog` plus `testbench_path` with iverilog and run it,
/// returning stdout. `-DSYNTHESIS` skips firtool's debug-only register-
/// randomization boilerplate, which uses an `automatic`-lifetime
/// construct Icarus doesn't implement; harmless to skip since our
/// registers are all properly reset (`regreset`) regardless.
fn simulate(verilog: &str, testbench_path: &str) -> String {
    let dir = tempdir();
    let design_path = dir.join("design.v");
    std::fs::write(&design_path, verilog).unwrap();
    let sim_path = dir.join("sim");

    let compile = Command::new("iverilog")
        .args(["-g2012", "-DSYNTHESIS", "-o"])
        .arg(&sim_path)
        .arg(testbench_path)
        .arg(&design_path)
        .output()
        .expect("failed to spawn iverilog");
    assert!(
        compile.status.success(),
        "iverilog failed:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new("vvp")
        .arg(&sim_path)
        .output()
        .expect("failed to spawn vvp");
    let _ = std::fs::remove_dir_all(&dir);
    String::from_utf8_lossy(&run.stdout).into_owned()
}

fn tempdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "trace-sim-test-{}-{}",
        std::process::id(),
        // No Instant needed: PID plus a static counter is unique enough
        // for a test binary that creates very few of these.
        {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        }
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn subleq_runs_and_computes_the_right_answer() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/subleq.tr"))
        .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/subleq_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: pc=6"),
        "pc did not settle at the halt address:\n{output}"
    );
    assert!(
        output.contains("mem[11]=5"),
        "mem[11] should hold 8 - 3 == 5:\n{output}"
    );
}

/// Proves fifo synthesis end to end: sim/fifo_bridge_tb.v checks both
/// the forward path (a value placed in `input` reaches `output`
/// unchanged) and backpressure (`transfer` stalls while `output` is
/// still full, then fires as soon as it drains) — the two failure
/// conditions DESIGN.md's opening example calls out by name.
#[test]
fn fifo_bridge_runs_and_backpressures() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/fifo_bridge.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/fifo_bridge_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
}

/// Proves memory is fully loadable and observable through ordinary
/// ports, no hierarchical peek/poke: sim/port_ram_tb.v writes distinct
/// words to two addresses through `addr`/`write_data`/`write_en` and
/// reads each back through `read_data` without aliasing. Compiles with
/// no `--disable-opt`, like accumulator.tr: `read_data` is a real
/// output port.
#[test]
fn port_ram_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/port_ram.tr"))
        .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/port_ram_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: read_data=abcd"),
        "expected address 5 to still hold 0xabcd:\n{output}"
    );
}

/// Proves real module ports end to end: sim/accumulator_tb.v drives
/// `inc` and reads `sum` through ordinary Verilog ports, no hierarchical
/// peek/poke, and compilation needs no `--disable-opt` — an observable
/// output is enough to keep firtool from DCE-ing the design.
#[test]
fn accumulator_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/accumulator.tr"
    ))
    .unwrap();
    let (tokens, lex_errors) = lexer::lex(&src);
    assert!(lex_errors.is_empty(), "{lex_errors:?}");
    let (ast, parse_errors) = parser::parse(&src, &tokens);
    assert!(parse_errors.is_empty(), "{parse_errors:?}");
    let (res, resolve_errors) = resolve::resolve(&ast);
    assert!(resolve_errors.is_empty(), "{resolve_errors:?}");
    let (fx, effect_errors) = effects::check(&ast, &res);
    assert!(effect_errors.is_empty(), "{effect_errors:?}");
    let (ty, type_errors) = types::check(&ast, &res);
    assert!(type_errors.is_empty(), "{type_errors:?}");
    let (sched, schedule_errors) = schedule::schedule(&ast, &res, &fx);
    assert!(schedule_errors.is_empty(), "{schedule_errors:?}");
    let fir = trace::firrtl::emit(&ast, &res, &fx, &ty, &sched)
        .unwrap_or_else(|e| panic!("emission failed: {e:?}"));

    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/accumulator_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: sum=26"),
        "sum did not accumulate correctly:\n{output}"
    );
}

/// Proves submodule instantiation end to end: sim/submodule_tb.v drives
/// `Top`'s `x`/`y` and reads `result` through ordinary ports — `Top`
/// `inst`-instantiates `Adder` and wires its ports (`adder.a := x`,
/// `result := adder.sum`), so this is also the first proof that reading a
/// child's output port and writing a child's input port compile to real,
/// working hardware, not just FIRRTL text firtool happens to accept.
#[test]
fn submodule_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/submodule.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/submodule_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: result=50"),
        "result did not settle at 20 + 30:\n{output}"
    );
}

/// Proves the widened expression surface (multiply, bitwise, static
/// shift, unary negate/complement, bit-select/slice) end to end:
/// sim/alu_tb.v picks a = 0xE3/b = 0x07 specifically because a's high
/// and low bits differ, so a shl/shr mix-up or a truncated (rather than
/// widening) multiply would show up as a wrong value, not just a design
/// firtool happens to accept.
#[test]
fn alu_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/alu.tr")).unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/alu_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains(
            "final: prod=635 band=3 bor=e7 bxor=e4 shl3=18 shr3=1c nega=1d nota=1c lo4=3 bit7=1"
        ),
        "final ALU outputs did not match expectations:\n{output}"
    );
}

/// Proves an instance port write nested in if/else reaches the child as a
/// mux, not a stale or dropped value: the `else` path must produce 0, not
/// whatever `x` happened to be left at.
#[test]
fn submodule_cond_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/submodule_cond.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/submodule_cond_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: result=0"),
        "result did not settle back at 0 once sel deselected the write:\n{output}"
    );
}

/// Proves per-port conflict precision through real simulation, not just
/// the derived schedule: `write_a` and `write_b` are two SEPARATE rules,
/// each driving a different port of the same instance. Under the old
/// whole-instance conflict model they'd conflict and only one would ever
/// fire; result=12 (not 5) is the direct evidence both fired this cycle.
#[test]
fn submodule_multi_port_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/submodule_multi_port.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/submodule_multi_port_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: result=12"),
        "both ports' writes should land the same cycle (5 + 7 = 12):\n{output}"
    );
}

/// Proves lexical module nesting through real simulation: examples/
/// submodule_nested.tr is submodule.tr's exact design with `Adder`
/// declared inside `Top`'s body instead of as a sibling — same ports, so
/// sim/submodule_tb.v (unmodified) drives it identically. FIRRTL has no
/// nested-module concept, so this should behave exactly like the flat
/// version; reusing the same testbench file is itself part of the proof.
#[test]
fn submodule_nested_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/submodule_nested.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/submodule_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: result=50"),
        "result did not settle at 20 + 30:\n{output}"
    );
}

/// Proves an inlined call to a user `fn` through real simulation, not
/// just that firtool accepts the emitted text. `--disable-opt` here is
/// for an unrelated reason from every other use of that flag in this
/// file: it avoids firtool hoisting the call body's shared subexpression
/// into an `automatic` variable inside the `always` block, a construct
/// Icarus rejects (see the comment on the matching tests/firrtl.rs test).
#[test]
fn call_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/call.tr")).unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/call_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: result=22"),
        "result did not settle at (200+100 mod 256)>>1 = 22:\n{output}"
    );
}

#[test]
fn call_branch_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/call_branch.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/call_branch_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: result=200"),
        "result did not settle at 200 (a > b branch):\n{output}"
    );
}

#[test]
fn call_writes_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/call_writes.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/call_writes_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: result=21 v_out=20"),
        "result/v_out did not settle at 21/20:\n{output}"
    );
}

#[test]
fn call_prio_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/call_prio.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/call_prio_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: grant=0"),
        "grant did not settle at 0:\n{output}"
    );
}
