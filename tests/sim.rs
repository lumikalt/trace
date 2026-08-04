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
use trace::{effects, elaborate, lexer, lower, parser, resolve, schedule, types};

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
    let (_ty, type_errors) = types::check(&ast, &res, &fx);
    assert!(type_errors.is_empty(), "{type_errors:?}");

    let (elab_edits, elab_errors) = elaborate::plan(&ast, &res, &fx, tr_src);
    assert!(elab_errors.is_empty(), "{elab_errors:?}");
    let elaborated_src = elaborate::render(tr_src, &elab_edits);

    let (tokens1, lex_errors1) = lexer::lex(&elaborated_src);
    assert!(lex_errors1.is_empty(), "{lex_errors1:?}\n{elaborated_src}");
    let (ast1, parse_errors1) = parser::parse(&elaborated_src, &tokens1);
    assert!(
        parse_errors1.is_empty(),
        "{parse_errors1:?}\n{elaborated_src}"
    );
    let (res1, resolve_errors1) = resolve::resolve(&ast1);
    assert!(
        resolve_errors1.is_empty(),
        "{resolve_errors1:?}\n{elaborated_src}"
    );
    let (fx1, effect_errors1) = effects::check(&ast1, &res1);
    assert!(
        effect_errors1.is_empty(),
        "{effect_errors1:?}\n{elaborated_src}"
    );
    let (ty1, type_errors1) = types::check(&ast1, &res1, &fx1);
    assert!(
        type_errors1.is_empty(),
        "{type_errors1:?}\n{elaborated_src}"
    );

    let (lowered, lower_errors) = lower::plan(&ast1, &res1, &fx1, &ty1);
    assert!(lower_errors.is_empty(), "{lower_errors:?}");
    let lowered_src = lower::render(&ast1, &elaborated_src, &lowered);

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
    let (ty2, type_errors2) = types::check(&ast2, &res2, &fx2);
    assert!(type_errors2.is_empty(), "{type_errors2:?}\n{lowered_src}");
    let (sched2, schedule_errors2) = schedule::schedule(&ast2, &res2, &fx2, &ty2);
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
        // Icarus compatibility, same spirit as `-DSYNTHESIS` below: a
        // cross-width intermediate (e.g. one `div`/`rem` operand zero-
        // extended to match the other's width before the primop runs)
        // otherwise lowers to an `automatic logic` declared INSIDE an
        // `always` block, which Icarus's `-g2012` rejects ("Overriding
        // the default variable lifetime is not yet supported") — first
        // hit by examples/div_rem.tr's differing-width case, since every
        // earlier example only ever crossed an always block with same-
        // width operands. This flag makes firtool lower the identical
        // intermediate to a plain top-level `wire` instead — same
        // design, Icarus-compatible Verilog — so it's applied
        // unconditionally rather than threaded through as a per-caller
        // flag.
        .arg("-lowering-options=disallowLocalVariables")
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

/// Same as `simulate`, plus one extra hand-written Verilog source file
/// compiled alongside the generated design — an `extmodule`'s own real
/// implementation, which trace's FIRRTL output never references (see
/// `ast::Item::ExtModule`'s doc comment: the `.v` path is opaque data,
/// entirely a downstream build/simulation concern). A separate fn rather
/// than threading an `extra_sources` param through `simulate`'s 47
/// existing call sites for this one, so-far-unique need.
fn simulate_with_blackbox(verilog: &str, testbench_path: &str, blackbox_path: &str) -> String {
    let dir = tempdir();
    let design_path = dir.join("design.v");
    std::fs::write(&design_path, verilog).unwrap();
    let sim_path = dir.join("sim");

    let compile = Command::new("iverilog")
        .args(["-g2012", "-DSYNTHESIS", "-o"])
        .arg(&sim_path)
        .arg(testbench_path)
        .arg(&design_path)
        .arg(blackbox_path)
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

/// Compile `verilog` plus `testbench_path` with Verilator and run the
/// result, returning stdout — the Verilator-backed twin of `simulate`.
/// `--binary --timing`: Verilator elaborates the hand-written SV
/// testbench itself as the simulation's own top (not a separate C++/DPI
/// harness reaching into a `--public`-exposed signal from outside) and
/// builds a standalone executable directly — the same "just run it" shape
/// `simulate`'s iverilog/vvp pair already has. `--timing` is what makes
/// this mode accept the testbenches' own `#5`/`@(posedge clock)` delays
/// and `initial` blocks at all — without it Verilator (synthesis-focused
/// by default) rejects most of that as unsupported. Because the testbench
/// and DUT elaborate as ONE design this way, a testbench's hierarchical
/// peeks/pokes (`dut.pc`, `dut.__fifo_input_valid = ...`, in
/// sim/subleq_tb.v/sim/fifo_bridge_tb.v) are ordinary intra-design SV
/// hierarchical references, not a C++-boundary crossing — confirmed
/// empirically (both run clean under this mode, with no
/// `--public`/`--public-flat-rw` and no Verilator warning about signal
/// visibility) — see DESIGN.md's/TODO.md's Verilator sections for the
/// correction to this project's earlier "would need --public" assumption.
/// `-DSYNTHESIS`: same reason `simulate` passes it to iverilog — kept for
/// parity with the generated Verilog, not because Verilator needs it (it
/// has no trouble with the `automatic`-lifetime debug-randomization block
/// Icarus rejects). `--top-module` is grepped from the testbench file's
/// own `module <name>` line rather than assumed from `testbench_path`'s
/// filename, the same defensive choice `devenv.nix`'s `simulate` script
/// makes for the identical reason.
fn simulate_verilator(verilog: &str, testbench_path: &str) -> String {
    simulate_verilator_impl(verilog, testbench_path, None)
}

/// Same as `simulate_verilator`, plus one extra hand-written Verilog
/// source compiled alongside — the Verilator-backed twin of
/// `simulate_with_blackbox`.
fn simulate_verilator_with_blackbox(
    verilog: &str,
    testbench_path: &str,
    blackbox_path: &str,
) -> String {
    simulate_verilator_impl(verilog, testbench_path, Some(blackbox_path))
}

fn simulate_verilator_impl(
    verilog: &str,
    testbench_path: &str,
    blackbox_path: Option<&str>,
) -> String {
    let dir = tempdir();
    let design_path = dir.join("design.v");
    std::fs::write(&design_path, verilog).unwrap();
    let mdir = dir.join("obj_dir");

    let tb_src = std::fs::read_to_string(testbench_path).unwrap();
    let top = tb_src
        .lines()
        .find_map(|l| l.trim_start().strip_prefix("module "))
        .and_then(|rest| {
            rest.split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .next()
        })
        .unwrap_or_else(|| panic!("no `module <name>` line found in {testbench_path}"))
        .to_string();

    let mut cmd = Command::new("verilator");
    cmd.args([
        "--binary",
        "--timing",
        "-DSYNTHESIS",
        "-Wno-fatal",
        "--top-module",
        &top,
        "--Mdir",
    ]);
    cmd.arg(&mdir).arg(&design_path).arg(testbench_path);
    if let Some(bb) = blackbox_path {
        cmd.arg(bb);
    }
    let compile = cmd.output().expect("failed to spawn verilator");
    assert!(
        compile.status.success(),
        "verilator failed:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let exe = mdir.join(format!("V{top}"));
    let run = Command::new(&exe)
        .output()
        .expect("failed to spawn the verilated binary");
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

/// Proves `while`'s multi-cycle lowering end to end: sim/while_
/// countdown_tb.v holds `x` at 5, 0, then 12 in turn and checks `iters`
/// settles at exactly `x` each time — real per-cycle iteration, not
/// just FIRRTL firtool happens to accept. `x=0` pins the zero-iteration
/// edge case (the loop's own condition already false the first time
/// it's checked).
#[test]
fn while_countdown_runs_through_real_cycles() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/while_countdown.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/while_countdown_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: iters=12"),
        "iters did not settle at the last driven x value:\n{output}"
    );
}

/// Proves `while`'s newly-relaxed BARE-comparison condition (no `logic`
/// needed, TODO.md's "four open questions" question 3) is truly
/// equivalent to the `logic`-wrapped form above, not just structurally
/// similar FIRRTL: `examples/while_countdown_bare.tr` is `while_
/// countdown.tr` with `logic` dropped, reusing the SAME testbench
/// (`sim/while_countdown_tb.v`, same module/port names) unchanged —
/// if the two forms compile to genuinely identical hardware, the
/// identical testbench passes with zero changes.
#[test]
fn while_countdown_bare_comparison_runs_through_real_cycles() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/while_countdown_bare.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/while_countdown_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: iters=12"),
        "iters did not settle at the last driven x value:\n{output}"
    );
}

/// Proves `break` genuinely exits a `while` loop early in real
/// hardware: `examples/while_break.tr` + `sim/while_break_tb.v` cover a
/// mid-loop break, an immediate break on the very first iteration, and
/// the loop never even being entered (the loop's OWN condition false
/// from the start, unrelated to `break`) — see `sim/while_break_tb.v`'s
/// own doc comment for the exact expected values and why one of them
/// isn't the naively-expected one (`break`'s own condition reads a
/// register's OLD value, same as any other read).
#[test]
fn while_break_exits_the_loop_early_through_real_cycles() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/while_break.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/while_break_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
}

/// The Verilator-backed twin of
/// `while_break_exits_the_loop_early_through_real_cycles`.
#[test]
fn while_break_exits_the_loop_early_through_real_cycles_under_verilator() {
    if !tool_available("firtool") || !tool_available("verilator") {
        eprintln!("firtool/verilator not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/while_break.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/while_break_tb.v");
    let output = simulate_verilator(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
}

/// Proves `while let`'s multi-cycle lowering end to end: sim/while_let_
/// drain_tb.v holds `x` at 5, 0, then 12 in turn and checks `iters`
/// settles at exactly `x` each time, the same shape `while_countdown`
/// pins for plain `while` — except the loop here is gated on a real
/// `opt_valid` register (`while let v = opt?`), not a comparison,
/// proving the rendered `if let`-as-self-loop text (`while_loop_header`,
/// lower.rs) actually re-enters `if let`'s own emission machinery
/// correctly, not just that firtool accepts the text.
#[test]
fn while_let_drain_runs_through_real_cycles() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/while_let_drain.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/while_let_drain_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: iters=12"),
        "iters did not settle at the last driven x value:\n{output}"
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

/// Proves examples/call_fifo.tr — fifo_bridge.tr's own transfer logic,
/// wrapped in a callee instead of written directly in the rule —
/// produces the IDENTICAL forward-path/backpressure handshaking through
/// real simulation, not just that firtool accepts the fold's emitted
/// text (pinned separately in tests/firrtl.rs against the unoptimized
/// FIRRTL). Exercises the harder composition: a fifo op reached through
/// a callee call, a `let`-bound callee-local threaded from a `Deq` into
/// an `Enq` within that same callee, and the callee's own return value
/// all agreeing on the same underlying register.
#[test]
fn call_fifo_bridges_through_a_callee_with_identical_handshaking() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/call_fifo.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/call_fifo_tb.v");
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

/// Proves the whole io/attach/extmodule feature set end to end, not just
/// that firtool accepts the emitted FIRRTL: sim/extmodule_tribuf_tb.v
/// instantiates TWO of examples/extmodule_tribuf.tr's `Top` sharing one
/// physical `bus` wire, alternates which side drives it, and checks the
/// OTHER side senses the driven value — a real bidirectional net,
/// neither direction fixed at compile time. sim/tribuf.v (compiled
/// alongside the testbench, not referenced by trace's own FIRRTL output
/// at all) supplies the `extmodule`'s actual tri-state implementation.
#[test]
fn extmodule_tribuf_runs_a_real_bidirectional_bus() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/extmodule_tribuf.tr"
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
    let (ty, type_errors) = types::check(&ast, &res, &fx);
    assert!(type_errors.is_empty(), "{type_errors:?}");
    let (sched, schedule_errors) = schedule::schedule(&ast, &res, &fx, &ty);
    assert!(schedule_errors.is_empty(), "{schedule_errors:?}");
    let fir = trace::firrtl::emit(&ast, &res, &fx, &ty, &sched)
        .unwrap_or_else(|e| panic!("emission failed: {e:?}"));

    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/extmodule_tribuf_tb.v");
    let blackbox = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/tribuf.v");
    let output = simulate_with_blackbox(&verilog, testbench, blackbox);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
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
    let (ty, type_errors) = types::check(&ast, &res, &fx);
    assert!(type_errors.is_empty(), "{type_errors:?}");
    let (sched, schedule_errors) = schedule::schedule(&ast, &res, &fx, &ty);
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

/// Proves `rule foo?`'s desugaring (TODO.md's "Rules: optional/enable
/// sugar") end to end, not just that it emits FIRRTL firtool accepts:
/// sim/optional_rule_tb.v holds `step` high straight through reset (must
/// NOT fire — `__prev_step` resets to `1`, the reset-edge semantic Lumi
/// picked, via `AskUserQuestion`), holds it high with no new edge (must
/// not re-fire), then drives two genuine 0->1 edges and checks `count`
/// increments exactly once per edge, not once per cycle the port is
/// held high.
#[test]
fn optional_rule_sugar_runs_through_real_reset_and_edges() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/optional_rule.tr"
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
    let (ty, type_errors) = types::check(&ast, &res, &fx);
    assert!(type_errors.is_empty(), "{type_errors:?}");
    let (sched, schedule_errors) = schedule::schedule(&ast, &res, &fx, &ty);
    assert!(schedule_errors.is_empty(), "{schedule_errors:?}");
    let fir = trace::firrtl::emit(&ast, &res, &fx, &ty, &sched)
        .unwrap_or_else(|e| panic!("emission failed: {e:?}"));

    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/optional_rule_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
}

/// Proves `?.` safe navigation's multi-hop guard-fold (TODO.md's "`?.`
/// safe navigation") end to end, not just that `fires_nav`'s FIRRTL text
/// contains both hops' `and(...)` terms: sim/optional_chain_tb.v drives
/// the one state that discriminates a genuine multi-hop fold from a
/// naive one that only checks the FINAL hop's own presence bit -- `a`
/// present but the intermediate `b` absent, which must NOT fire `nav`
/// even though `a` itself is present -- then a fully-present chain
/// (fires every cycle, reads the right value), then fully absent again
/// (stops firing). A bug that folded only `b.valid` would let the first
/// state fire wrongly; a bug that folded only `a.valid` would never be
/// caught by ANY reachable state in this language (every write to `a`
/// sets every flattened leaf together, so `a.valid=0` with a STALE
/// `b.valid=1` isn't reachable here) -- see checks.rs's `guards_
/// outside_allowed_positions` for why that asymmetry is fine.
#[test]
fn optional_chain_sugar_runs_through_a_genuinely_absent_intermediate_hop() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/optional_chain.tr"
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
    let (ty, type_errors) = types::check(&ast, &res, &fx);
    assert!(type_errors.is_empty(), "{type_errors:?}");
    let (sched, schedule_errors) = schedule::schedule(&ast, &res, &fx, &ty);
    assert!(schedule_errors.is_empty(), "{schedule_errors:?}");
    let fir = trace::firrtl::emit(&ast, &res, &fx, &ty, &sched)
        .unwrap_or_else(|e| panic!("emission failed: {e:?}"));

    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/optional_chain_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
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

/// Proves a callee's bare guard actually gates the caller's rule
/// through real simulation, not just that firtool accepts the fold's
/// emitted `neq(a, 0)` guard string (that part is pinned in
/// tests/firrtl.rs, against the unoptimized FIRRTL text). `--disable-
/// opt` for the same `automatic`-lifetime reason `call_runs_through_
/// real_ports` needs it.
#[test]
fn call_guard_folds_the_callees_guard_through_real_simulation() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/call_guard.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/call_guard_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: result=7"),
        "expected result to hold at 7:\n{output}"
    );
}

#[test]
fn call_nested_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/call_nested.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/call_nested_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: result=6"),
        "result did not settle at (130+1)*2 mod 256 = 6:\n{output}"
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
fn call_nested_writes_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/call_nested_writes.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/call_nested_writes_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: v_out=21"),
        "v_out did not settle at 21 (a+1, via Outer->Inner):\n{output}"
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

#[test]
fn call_trunc_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/call_trunc.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/call_trunc_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: result=239"),
        "result did not settle at 239 (0xef, the low byte of 0xbeef):\n{output}"
    );
}

#[test]
fn call_pack_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/call_pack.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/call_pack_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: result=43707"),
        "result did not settle at 43707 (0xaabb, a in the high byte):\n{output}"
    );
}

#[test]
fn sized_literal_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/sized_literal.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/sized_literal_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: result=14 bit3=1"),
        "result/bit3 did not settle at 14/1:\n{output}"
    );
}

#[test]
fn infer_reg_ty_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/infer_reg_ty.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/infer_reg_ty_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: sum=16 hi=ff00"),
        "sum/hi did not settle at 16/ff00:\n{output}"
    );
}

/// Proves `/`/`%` end to end: `a` ([8]) = 200, `b` ([4]) = 13,
/// deliberately different widths so both the pad and no-pad branches of
/// compile_binop's Div/Rem arms actually run, not just the equal-width
/// case tests/firrtl.rs's unit tests already pin.
#[test]
fn div_rem_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/div_rem.tr"))
        .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/div_rem_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: q1=15 q2=0 r1=5 r2=13"),
        "q1/q2/r1/r2 did not settle at 15/0/5/13 (200/13, 13/200, 200%13, 13%200):\n{output}"
    );
}

/// Proves dynamic-amount `<<`/`>>` end to end: `x` (0xE3) stays fixed
/// while `n` changes between two different values across two cycles,
/// proving the shift amount is genuinely read at runtime, not baked in
/// at synthesis time. Both results at each `n` match the exact values
/// alu.tr's static `<<3`/`>>3` already proved correct, confirming the
/// dynamic path computes the identical answer a literal shift would.
#[test]
fn dynamic_shift_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/dynamic_shift.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/dynamic_shift_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: n3 shl=18 shr=1c; n5 shl=60 shr=7"),
        "dynamic shift results did not match expectations:\n{output}"
    );
}

/// Proves `>>>` (arithmetic, sign-extending right shift) genuinely
/// differs from `>>` (logical, zero-filling) for a value with its sign
/// bit set, both statically and dynamically shifted -- see
/// sim/arith_shift_tb.v's own comment.
#[test]
fn arith_shift_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/arith_shift.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/arith_shift_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: static shr=1c ashr=fc; n5 shr=7 ashr=ff"),
        "arithmetic shift results did not match expectations:\n{output}"
    );
}

/// Proves all three dynamic bit-select shapes end to end: a single index
/// (`x[i]`), and Verilog-style indexed part-select (`x[i +: 4]`/
/// `x[i -: 4]`). `x` stays fixed at 0xE3 while `i` changes between two
/// values across two cycles, and the second (`i=7`) deliberately runs
/// `up` past the top of `x`'s own 8 bits to prove the dshr-based
/// implementation's zero-padding beyond the original width is correct,
/// not just the always-in-range case.
#[test]
fn dynamic_bit_select_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/dynamic_bit_select.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/dynamic_bit_select_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: i4 bit=0 up=e down=1; i7 bit=1 up=1 down=e"),
        "dynamic bit-select results did not match expectations:\n{output}"
    );
}

#[test]
fn mem_write_branch_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/mem_write_branch.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/mem_write_branch_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: count=10 read_data=aa"),
        "mem-write-branch results did not match expectations:\n{output}"
    );
}

#[test]
fn mutually_exclusive_check_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/mutually_exclusive_check.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/sim/mutually_exclusive_check_tb.v"
    );
    let output = simulate(&verilog, testbench);

    // The whole point of this test: the compiler-inserted assertion must
    // be a genuine runtime check, not dead code that always passes. Split
    // on the testbench's own "safe window done" marker and check the
    // claim-violation message appears only AFTER it, never before. (Not
    // checking for "SIMULATION PASSED" here — the testbench prints it
    // unconditionally at the end regardless of outcome, since the whole
    // point of the violated window is to trigger the assertion; that
    // check would be vacuous.)
    let marker = "SAFE WINDOW DONE";
    let split = output.find(marker).unwrap_or_else(|| {
        panic!("testbench did not print its own \"{marker}\" marker:\n{output}")
    });
    let (safe_window, rest) = output.split_at(split);
    let violation_message = "mutually_exclusive claim violated: rule set_a and rule set_b both \
                              fired the same cycle";
    assert!(
        !safe_window.contains(violation_message),
        "assertion fired during the SAFE window (false positive):\n{output}"
    );
    assert!(
        rest.contains(violation_message),
        "assertion did NOT fire during the deliberately-violated window (dead check):\n{output}"
    );
}

#[test]
fn conflict_free_mem_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/conflict_free_mem.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/conflict_free_mem_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: read_data=3333"),
        "conflict-free-mem results did not match expectations:\n{output}"
    );
}

#[test]
fn conflict_free_mem_checked_assertion_catches_address_collision() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/conflict_free_mem.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/sim/conflict_free_mem_collision_tb.v"
    );
    let output = simulate(&verilog, testbench);

    // The whole point of this test: `conflict_free`'s stated precondition
    // ("a write and a read to DIFFERENT addresses") is now a genuine
    // runtime check, not just a trusted claim — a window where it
    // actually holds produces no assertion failure, while a window that
    // deliberately collides the two addresses DOES.
    let marker = "SAFE WINDOW DONE";
    let split = output.find(marker).unwrap_or_else(|| {
        panic!("testbench did not print its own \"{marker}\" marker:\n{output}")
    });
    let (safe_window, rest) = output.split_at(split);
    let violation_message = "conflict_free claim violated: rule read and rule write accessed \
                              the same address in `m` the same cycle";
    assert!(
        !safe_window.contains(violation_message),
        "assertion fired during the SAFE window (false positive):\n{output}"
    );
    assert!(
        rest.contains(violation_message),
        "assertion did NOT fire during the deliberately-colliding window (dead check):\n{output}"
    );
}

#[test]
fn fifo_passthrough_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/fifo_passthrough.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/fifo_passthrough_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: last_out=14"),
        "fifo-passthrough results did not match expectations:\n{output}"
    );
}

/// A depth-3 fifo (`[3][8]`, non-power-of-2 to stress head/tail
/// wraparound): fills to full, exercises a full-buffer combined
/// Enq+Deq pass-through, drains to empty, then refills and drains a
/// second time so the internal pointers wrap past the top slot index.
/// sim/fifo_depth_tb.v asserts every dequeued value comes back in
/// exact FIFO order (10, 20, 30, 40, 50, 60, 70) -- this shape was
/// first hand-verified against a raw FIRRTL circuit (not generated by
/// this compiler) through real firtool + Icarus before being ported
/// into module.rs's codegen; this test is what actually exercises that
/// codegen.
#[test]
fn fifo_depth_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/fifo_depth.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/fifo_depth_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: last_deq=70"),
        "fifo-depth results did not match expectations:\n{output}"
    );
}

/// Proves the SUBLEQ boot-load design end to end: unlike
/// `subleq_runs_and_computes_the_right_answer` above (which pokes
/// `mem` directly before reset even clears), this loads the identical
/// program through real load_addr/load_data/load_en ports while
/// `booted == 0`, with a deliberate idle gap in the MIDDLE of loading
/// (memory only half-written), then pulses `boot_done` and expects
/// the same correct halt state. sim/subleq_boot_tb.v's own doc
/// comment records that this test was confirmed to actually fail (not
/// vacuously pass) against a variant with the `booted` guard removed
/// from `step`/`refill`, before being trusted as a real regression
/// test.
#[test]
fn subleq_boot_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/subleq_boot.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    // No output ports (only the loader side gained ports), so, same
    // as subleq.tr, firtool would DCE the whole design without this.
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/subleq_boot_tb.v");
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

/// Proves reassigned-local resolution through real hardware, not just
/// emitted FIRRTL text. sim/reassigned_local_tb.v's own doc comment
/// records that a deliberately broken variant (`x` bound only once, to
/// `b`) was re-run against this exact testbench and failed as
/// predicted (`first_val` reading `b` instead of `a`), confirming this
/// test genuinely discriminates position-correct resolution from a
/// naive last-assignment-wins compile.
#[test]
fn reassigned_local_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/reassigned_local.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/reassigned_local_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: first_val=255 second_val=128"),
        "reassigned-local results did not match expectations:\n{output}"
    );
}

/// Proves `spawn`/`sync` end to end: DESIGN.md's `Fetch2` example, two
/// independent bank-read FSMs running in parallel off distinct
/// `__cont_*` registers, triggered together and joined by `sync` before
/// their results are packed. See sim/fetch2_tb.v's own comment for how
/// this was confirmed to actually discriminate a missing trigger-time
/// argument save, not just pass vacuously.
#[test]
fn fetch2_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/fetch2.tr"))
        .unwrap();
    let fir = generate_firrtl(&src);
    // `bank0`/`bank1` are never WRITTEN anywhere in this design (read-only
    // banks, no loader rule like subleq_boot.tr's) -- same as
    // checksum.tr, firtool's default optimizer treats that as license to
    // constant-fold the whole read/pack/`ir` chain away despite the real
    // output port, confirmed empirically.
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/fetch2_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: ir=aaaabbbb"),
        "ir did not settle at the packed trigger-time bank reads:\n{output}"
    );
}

/// Companion to `fetch2_runs_through_real_ports`: the trigger sits
/// behind an extra guard (`go`), held low for several cycles after
/// reset. See sim/fetch2_gated_tb.v's own comment for why this proves
/// the spawn lowering's plain-0 continuation-register reset (as opposed
/// to a sentinel/idle value, considered and then found unnecessary) is
/// safe for a delayed trigger too, not just the immediate-trigger shape
/// `fetch2.tr` exercises.
#[test]
fn fetch2_gated_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/fetch2_gated.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    // Same as fetch2.tr: bank0/bank1 are never written, so firtool's
    // optimizer constant-folds the read/pack/`ir` chain away without
    // this despite the real output port.
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/fetch2_gated_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: ir=aaaabbbb"),
        "ir did not settle at the packed trigger-time bank reads:\n{output}"
    );
}

/// Proves `race` actually cancels its loser, end to end: Fast (1 tick)
/// always beats Slow (2 ticks); this checks Slow's own internal
/// `done`/`result` registers directly (hierarchical path), not just the
/// `out` port, since `out` alone can't distinguish true cancellation
/// from "the loser ran to completion anyway, but nothing happened to
/// read its result" -- see sim/race_tb.v's own comment.
#[test]
fn race_cancels_the_loser() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/race.tr")).unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/race_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
}

/// Proves the value-producing `race` form (`out := tick race[ha, hb]`)
/// end to end: A and B both take exactly one tick, so they'd be READY
/// the same cycle, but the cancellation mechanism's own mutual
/// conflict means the scheduler always picks exactly one to actually
/// complete (A, by spawn declaration order) -- `out` reflects THAT
/// real winner, never a value from a handle that never finished. This
/// is NOT evidence of `__race_value`'s own mux-level tie-break (that
/// code path is structurally unreachable for handles racing each
/// other, given the cancellation guarantee already rules out both
/// being done at once) -- see sim/race_value_tb.v's own comment, which
/// checks this directly (`done_hb` stays 0), not just that `out` came
/// out right.
#[test]
fn race_value_reflects_the_scheduler_decided_winner() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/race_value.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/race_value_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
}

/// Proves the `Checksum` motivating example (unrolled accumulation via
/// a local reassigned four times in a row) through real firtool and
/// Icarus, not just plausible-looking FIRRTL text. See
/// sim/checksum_tb.v's own comment for why this needs `--disable-opt`
/// despite having a real output port.
#[test]
fn checksum_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/checksum.tr"))
        .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/checksum_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: result=100"),
        "checksum result did not match expectations:\n{output}"
    );
}

/// Proves the scheduler's own auto-derived mem-disjointness proof
/// (schedule.rs's `Exemption::Disjoint`) through real firtool and
/// Icarus, not just `--explain-schedule`'s own text. `write` and
/// `read` are both unconditional and share no `conflict_free`
/// annotation at all -- under the old, fully conservative model,
/// `read` would never get a turn (see sim/mem_disjoint_rw_tb.v's own
/// comment for the exact regression this catches: the old behavior
/// silently DCEs the whole memory reader down to a constant 0). Real
/// output ports keep this alive without `--disable-opt`, same as
/// conflict_free_mem.tr / accumulator.tr.
#[test]
fn mem_disjoint_rw_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/mem_disjoint_rw.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/mem_disjoint_rw_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: write_count=5 read_count=5 y=aa Memory[3]=42"),
        "mem_disjoint_rw result did not match expectations:\n{output}"
    );
}

/// v2 of the test above: proves schedule.rs's affine-offset proof
/// (`m[i]` vs `m[i+1]`, same base register, power-of-two depth) through
/// real firtool + Icarus, including the wraparound boundary (`i=15`,
/// `i+1` truncates to `0`) that specifically exercises the proof's own
/// modular-arithmetic soundness argument, not just "two addresses
/// happen to differ."
#[test]
fn mem_disjoint_affine_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/mem_disjoint_affine.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/mem_disjoint_affine_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: Memory[5]=11 Memory[6]=aa Memory[15]=22 Memory[0]=bb y=bb"),
        "mem_disjoint_affine result did not match expectations:\n{output}"
    );
}

/// v3 of the mem-disjointness proof: `write`/`read` share a power-of-two
/// multiplier (`2*i` vs `2*j+1`) but TWO GENUINELY DIFFERENT bases,
/// unlike v1/v2's same-address/same-base cases -- base identity drops
/// out of the argument entirely (`2*x` is always even for ANY x), so no
/// range tracking is needed even though `i` and `j` are unrelated
/// inputs. The testbench specifically drives `i == j` on one cycle to
/// prove the argument holds even when the two bases coincide, not just
/// when they happen to differ.
#[test]
fn mem_disjoint_banked_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/mem_disjoint_banked.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/mem_disjoint_banked_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: write_count=3 read_count=3 y=cc Memory[0]=22 Memory[10]=33"),
        "mem_disjoint_banked result did not match expectations:\n{output}"
    );
}

/// Proves DESIGN.md's own `<elaborates>` example (`AdderTree`, compile-
/// time tree recursion over a `list` with one-sided slices) through real
/// firtool and Icarus: `elaborate.rs`'s pre-pass unrolls `AdderTree([a,
/// b, c, d])` into `((a + b) + (c + d))` entirely at compile time, and
/// the resulting hardware both sums correctly and wraps modularly past
/// `[32]` — see sim/adder_tree_tb.v.
#[test]
fn adder_tree_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/adder_tree.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/adder_tree_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
}

/// Proves `logic <expr>` end to end: `examples/logic_probe.tr`'s `probe`
/// rule reads a fifo's occupancy and a guard-only `<fails>` call's
/// condition as plain status outputs, with neither side effect (no real
/// dequeue, no callee write) — `drain`, a SEPARATE rule doing the real
/// dequeue, coexists in the same cycle (`conflict_free`) without either
/// rule disturbing the other. See sim/logic_probe_tb.v.
#[test]
fn logic_probe_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/logic_probe.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/logic_probe_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
}

/// Proves `or` end to end: `examples/or_fifos.tr`'s `pick_default` chain
/// (ends in a default) fires every cycle regardless of fifo occupancy;
/// `pick_strict` (no default) stays fallible and stalls when neither
/// alternative is ready. Both pick in priority order and leave the loser
/// fifo untouched. See sim/or_fifos_tb.v.
#[test]
fn or_fifos_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/or_fifos.tr"))
        .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/or_fifos_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
}

/// Proves `if let x = fifo.Deq[] { ... }` end to end:
/// `examples/if_let_fifo.tr`'s `consumer` rule fires every cycle
/// (`if let` never gates the rule, same as its Option-presence sibling),
/// but only actually dequeues on the one cycle `f` has data -- `result`/
/// `was_present` show the pushed value exactly one cycle after the push,
/// absent every other cycle. See sim/if_let_fifo_tb.v.
#[test]
fn if_let_fifo_deq_drives_a_real_dequeue_exactly_when_present() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/if_let_fifo.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/if_let_fifo_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
}

/// The Verilator-backed twin of
/// `if_let_fifo_deq_drives_a_real_dequeue_exactly_when_present`.
#[test]
fn if_let_fifo_deq_drives_a_real_dequeue_exactly_when_present_under_verilator() {
    if !tool_available("firtool") || !tool_available("verilator") {
        eprintln!("firtool/verilator not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/if_let_fifo.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/if_let_fifo_tb.v");
    let output = simulate_verilator(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
}

/// Proves `if let x = Classify(a) { ... }` end to end:
/// `examples/if_let_failing_call.tr`'s `compute` rule fires every cycle
/// (`if let` never gates the rule), and `result`/`was_present` track
/// `Classify`'s own guard exactly across both an absent (`a = 0`) and
/// present (`a` nonzero) input. See sim/if_let_failing_call_tb.v.
#[test]
fn if_let_failing_call_tracks_the_callees_own_guard() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/if_let_failing_call.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/if_let_failing_call_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
}

/// The Verilator-backed twin of
/// `if_let_failing_call_tracks_the_callees_own_guard`.
#[test]
fn if_let_failing_call_tracks_the_callees_own_guard_under_verilator() {
    if !tool_available("firtool") || !tool_available("verilator") {
        eprintln!("firtool/verilator not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/if_let_failing_call.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/if_let_failing_call_tb.v");
    let output = simulate_verilator(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
}

/// Proves `if f.Deq[] { ... }` end to end -- a fifo op used DIRECTLY as
/// a bare `if`'s own condition, no bound name: `examples/if_bare_fifo.tr`'s
/// `consumer` fires every cycle, but `was_present` tracks the fifo's own
/// occupancy exactly, the same shape `if_let_fifo_deq_drives_a_real_
/// dequeue_exactly_when_present` proves for the bound-name form. See
/// sim/if_bare_fifo_tb.v.
#[test]
fn if_bare_fifo_deq_condition_drives_a_real_dequeue_exactly_when_present() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/if_bare_fifo.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/if_bare_fifo_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
}

/// The Verilator-backed twin of
/// `if_bare_fifo_deq_condition_drives_a_real_dequeue_exactly_when_present`.
#[test]
fn if_bare_fifo_deq_condition_drives_a_real_dequeue_exactly_when_present_under_verilator() {
    if !tool_available("firtool") || !tool_available("verilator") {
        eprintln!("firtool/verilator not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/if_bare_fifo.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/if_bare_fifo_tb.v");
    let output = simulate_verilator(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
}

/// Proves the branch-mutual-exclusivity feature end to end
/// (TODO.md's "four open questions", question 1): `examples/branch_fifo_
/// deq.tr` has two `Deq[]`s on the SAME fifo, one in `then` and one in
/// `else` of the identical `if` -- each routes to a different output,
/// and `sim/branch_fifo_deq_tb.v` confirms they're really mutually
/// exclusive real hardware (one fires, never both, and the un-taken
/// branch's output holds its prior value), not just a structural FIRRTL
/// claim that happens to type-check.
#[test]
fn two_deqs_on_the_same_fifo_in_then_and_else_are_really_mutually_exclusive() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/branch_fifo_deq.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/branch_fifo_deq_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
}

/// The Verilator-backed twin of
/// `two_deqs_on_the_same_fifo_in_then_and_else_are_really_mutually_exclusive`.
#[test]
fn two_deqs_on_the_same_fifo_in_then_and_else_are_really_mutually_exclusive_under_verilator() {
    if !tool_available("firtool") || !tool_available("verilator") {
        eprintln!("firtool/verilator not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/branch_fifo_deq.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/branch_fifo_deq_tb.v");
    let output = simulate_verilator(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
}

/// Proves `if Classify(a) { ... }` end to end -- a failing call used
/// DIRECTLY as a bare `if`'s own condition, no bound name:
/// `examples/if_bare_failing_call.tr`'s `was_present` tracks `Classify`'s
/// own guard exactly, the same shape `if_let_failing_call_tracks_the_
/// callees_own_guard` proves for the bound-name form. See
/// sim/if_bare_failing_call_tb.v.
#[test]
fn if_bare_failing_call_condition_tracks_the_callees_own_guard() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/if_bare_failing_call.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/if_bare_failing_call_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
}

/// The Verilator-backed twin of
/// `if_bare_failing_call_condition_tracks_the_callees_own_guard`.
#[test]
fn if_bare_failing_call_condition_tracks_the_callees_own_guard_under_verilator() {
    if !tool_available("firtool") || !tool_available("verilator") {
        eprintln!("firtool/verilator not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/if_bare_failing_call.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/if_bare_failing_call_tb.v");
    let output = simulate_verilator(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
}

/// Proves general structs end to end: `examples/struct_pair.tr`'s
/// struct-typed reg `p` and struct-typed output `result` both flatten
/// to per-field registers/ports, a struct literal write (`p :=
/// Pair{...}`) updates both fields together in one cycle, and reading
/// two fields off the same struct-typed reg into a fresh literal
/// (`result := Pair{valid: p.valid, data: p.data}`) round-trips both
/// values. See sim/struct_pair_tb.v.
#[test]
fn struct_pair_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/struct_pair.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/struct_pair_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: p_valid=1 p_data=2a result_valid=1 result_data=2a"),
        "p/result did not settle at valid=1 data=0x2a:\n{output}"
    );
}

/// Proves NESTED structs end to end: `examples/struct_nested.tr`'s
/// `Frame { header: Header, data: [8] }` flattens all the way down
/// to per-leaf-field registers/ports (`f_header_valid`, `f_header_seq`,
/// `f_data`), a nested struct literal write updates every leaf field
/// together in one cycle, and a chained field read (`f.header.valid`)
/// resolves through both levels into a freshly-built nested literal.
/// See sim/struct_nested_tb.v.
#[test]
fn struct_nested_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/struct_nested.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/struct_nested_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains(
            "final: f_header_valid=1 f_header_seq=0 f_data=2a result_header_valid=1 \
             result_header_seq=0 result_data=2a"
        ),
        "f/result did not settle correctly:\n{output}"
    );
}

/// Proves `?T` option types end to end: `examples/option.tr`'s `opt`
/// resets to absent (`false`), a fifo-fed value coerces implicitly to
/// present, the `?` unwrap (`result := opt?`) only takes effect on a
/// cycle `opt` is actually present (its guard folds into `pass`'s own
/// rule guard), the non-failing `.valid`/`.data` if/else escape hatch
/// (`check`) tracks presence independently, and an `?T`-typed OUTPUT
/// port (`relayed`) write-threads correctly through its own separate
/// bookkeeping. See sim/option_tb.v.
#[test]
fn option_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/option.tr"))
        .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/option_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains(
            "final: opt_valid=1 opt_data=2a result=2a was_present=1 relayed_valid=1 \
             relayed_data=2a"
        ),
        "opt/result/was_present/relayed did not settle correctly:\n{output}"
    );
}

/// Proves struct/`?T`-typed fn PARAMS end to end:
/// `examples/call_struct_param.tr`'s `UsePair`/`Consume` callees each
/// resolve a REG-typed argument (not just a literal) by chasing
/// through the param binding to the reg's own flat field registers,
/// and `Consume`'s bare-statement guard (`o?`) folds through that same
/// param binding into the caller's own rule guard. See
/// sim/call_struct_param_tb.v.
#[test]
fn call_struct_param_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/call_struct_param.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/call_struct_param_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: from_pair=2a from_opt= 42"),
        "from_pair/from_opt did not settle correctly:\n{output}"
    );
}

/// The return-side twin of `call_struct_param_runs_through_real_ports`:
/// `MakePair`'s struct return and `Wrap`'s `?T` return (a `<fails>`
/// callee whose guard folds into `fill_opt`'s own rule guard) both
/// decompose correctly through real ports/fifos, AND a zero item
/// (Wrap's guard failing) stays stuck in `opt_input` rather than
/// silently overwriting `opt` with a bogus present(0). See
/// sim/call_struct_return_tb.v.
#[test]
fn call_struct_return_runs_through_real_ports() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/call_struct_return.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/call_struct_return_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: from_pair=2a from_opt= 42"),
        "from_pair/from_opt did not settle correctly:\n{output}"
    );
}

// --- Verilator backend ------------------------------------------------
//
// Not a full second copy of every test above: functional correctness is
// already fully proven by the iverilog suite this file otherwise is.
// What's new here is interface PORTABILITY — does the same FIRRTL/
// testbench pair also run under a completely different simulator — and
// three tests, one per genuinely distinct access shape, carry that:
// plain module ports (`accumulator`), hierarchical peek/poke including a
// nested submodule's own memory array (`subleq`), and an extmodule
// blackbox compiled alongside the design (`extmodule_tribuf`). Every
// example not covered here differs from these three only in which
// LANGUAGE FEATURE it exercises (structs, `spawn`, `while`, ...), which
// affects the FIRRTL firtool produces, not the simulator interface — so
// it doesn't need its own Verilator-specific test. See `simulate_
// verilator`'s own doc comment for the mechanism, and DESIGN.md's/
// TODO.md's Verilator sections for the full story, including the
// correction to this project's earlier "would need --public/
// --public-flat-rw" assumption.

/// The Verilator-backed twin of `accumulator_runs_through_real_ports`.
#[test]
fn accumulator_runs_through_real_ports_under_verilator() {
    if !tool_available("firtool") || !tool_available("verilator") {
        eprintln!("firtool/verilator not on PATH; skipping (run via `devenv shell` or `t`)");
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
    let (ty, type_errors) = types::check(&ast, &res, &fx);
    assert!(type_errors.is_empty(), "{type_errors:?}");
    let (sched, schedule_errors) = schedule::schedule(&ast, &res, &fx, &ty);
    assert!(schedule_errors.is_empty(), "{schedule_errors:?}");
    let fir = trace::firrtl::emit(&ast, &res, &fx, &ty, &sched)
        .unwrap_or_else(|e| panic!("emission failed: {e:?}"));

    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/accumulator_tb.v");
    let output = simulate_verilator(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: sum=26"),
        "sum did not accumulate correctly:\n{output}"
    );
}

/// The Verilator-backed twin of `subleq_runs_and_computes_the_right_
/// answer`. Its testbench (`sim/subleq_tb.v`) is the strongest
/// hierarchical-reference case in this repo: a flat `dut.pc` peek AND a
/// nested submodule's own memory array (`dut.m_ext.Memory[i]`) — the
/// exact shape `--public`/`--public-flat-rw` govern under a C++/DPI
/// harness, and exactly what `--binary --timing` sidesteps by keeping
/// the testbench and DUT in one SV design instead.
#[test]
fn subleq_runs_and_computes_the_right_answer_under_verilator() {
    if !tool_available("firtool") || !tool_available("verilator") {
        eprintln!("firtool/verilator not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/subleq.tr"))
        .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/subleq_tb.v");
    let output = simulate_verilator(&verilog, testbench);

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

/// The Verilator-backed twin of `extmodule_tribuf_runs_a_real_
/// bidirectional_bus`: a second, hand-written Verilog source
/// (`sim/tribuf.v`) compiled alongside the generated design, proving the
/// blackbox-extmodule wiring works under this backend too.
#[test]
fn extmodule_tribuf_runs_a_real_bidirectional_bus_under_verilator() {
    if !tool_available("firtool") || !tool_available("verilator") {
        eprintln!("firtool/verilator not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/extmodule_tribuf.tr"
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
    let (ty, type_errors) = types::check(&ast, &res, &fx);
    assert!(type_errors.is_empty(), "{type_errors:?}");
    let (sched, schedule_errors) = schedule::schedule(&ast, &res, &fx, &ty);
    assert!(schedule_errors.is_empty(), "{schedule_errors:?}");
    let fir = trace::firrtl::emit(&ast, &res, &fx, &ty, &sched)
        .unwrap_or_else(|e| panic!("emission failed: {e:?}"));

    let verilog = firrtl_to_verilog(&fir, false);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/extmodule_tribuf_tb.v");
    let blackbox = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/tribuf.v");
    let output = simulate_verilator_with_blackbox(&verilog, testbench, blackbox);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
}

// --- Testbenches written in trace ---------------------------------------
//
// examples/tb_accumulator.tr's `Top` is not a feature demo the way every
// other example is -- it's a testbench: it `inst`s `Accumulator` (the
// same DUT examples/accumulator.tr uses), drives it across real clock
// cycles via a `<sequences>` rule, and checks the result with a bare
// comparison (`dut.sum = 26`) -- needing no new language surface at all,
// stimulus OR check. sim/tb_accumulator_tb.v's own job shrinks
// accordingly: just clock/reset generation and a look at whether the
// `<sequences>` continuation register wrapped back to 0 (checkpoint
// held) or stuck at a nonzero segment (checkpoint failed) -- see both
// files' own comments for the full story.

/// Proves a trace-authored testbench drives a real DUT correctly under
/// iverilog: `Top`'s `drive` rule should run its full `<sequences>`
/// cycle (4 cycles of `inc = 5` plus 2 of `inc = 3`) and its final
/// checkpoint (`dut.sum = 26`) should hold, letting `__cont_drive` wrap
/// back to 0 -- the same expected total
/// `accumulator_runs_through_real_ports` checks via a hand-written
/// stimulus sequence instead.
#[test]
fn tb_accumulator_drives_a_dut_through_real_cycles() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/tb_accumulator.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/tb_accumulator_tb.v");
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: __cont_drive=0"),
        "drive did not run to completion:\n{output}"
    );
}

/// The negative case: examples/tb_accumulator_failing.tr's checkpoint
/// (`dut.sum = 99`) can never hold, so `drive`'s `<sequences>`
/// continuation register should stick at its last segment (6) forever
/// instead of wrapping back to 0 -- proving a failing checkpoint is
/// actually observable from outside, not just a claim about how
/// `comparison_conds`/guard-folding ought to behave.
#[test]
fn tb_accumulator_checkpoint_failure_stalls_and_is_observable() {
    if !tool_available("firtool") || !tool_available("iverilog") {
        eprintln!("firtool/iverilog not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/tb_accumulator_failing.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/sim/tb_accumulator_failing_tb.v"
    );
    let output = simulate(&verilog, testbench);

    assert!(
        output.contains("SIMULATION FAILED: stalled at segment 6"),
        "failing checkpoint should have stalled drive at segment 6:\n{output}"
    );
}

/// The Verilator-backed twin of `tb_accumulator_drives_a_dut_through_
/// real_cycles`.
#[test]
fn tb_accumulator_drives_a_dut_through_real_cycles_under_verilator() {
    if !tool_available("firtool") || !tool_available("verilator") {
        eprintln!("firtool/verilator not on PATH; skipping (run via `devenv shell` or `t`)");
        return;
    }
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/tb_accumulator.tr"
    ))
    .unwrap();
    let fir = generate_firrtl(&src);
    let verilog = firrtl_to_verilog(&fir, true);
    let testbench = concat!(env!("CARGO_MANIFEST_DIR"), "/sim/tb_accumulator_tb.v");
    let output = simulate_verilator(&verilog, testbench);

    assert!(
        output.contains("SIMULATION PASSED"),
        "simulation did not report PASSED:\n{output}"
    );
    assert!(
        output.contains("final: __cont_drive=0"),
        "drive did not run to completion:\n{output}"
    );
}
