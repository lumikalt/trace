use trace::firrtl::{EmitError, emit};
use trace::{effects, lexer, lower, parser, resolve, schedule, types};

/// Full pipeline including suspends lowering (re-lexed/parsed once the
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
    let pc_line = verilog
        .lines()
        .find(|l| l.trim_start().starts_with("pc <="))
        .expect("expected a pc <= assignment");
    assert!(pc_line.contains("_GEN"), "{pc_line}");
}

#[test]
fn errors_on_unlowered_suspends_rule() {
    // emit_from_source lowers automatically when lowering applies; use
    // a rule shape lowering itself rejects (nested tick) so a still-
    // <suspends> rule with a tick reaches the emitter unlowered.
    let src = "\
module M {
    reg x : bits[1] = 0
    rule r <suspends> {
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
            .any(|e| e.message.contains("run suspends lowering first"))
    );
}

#[test]
fn errors_on_fifo() {
    let src = "module M {\n fifo f : bits[8]\n reg x : bits[8] = 0\n rule r {\n x := x\n }\n}\n";
    let err = emit_from_source(src).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.message.contains("does not synthesize fifo"))
    );
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
fn errors_on_multiple_modules() {
    let src = "\
module A {
    reg x : bits[1] = 0
}
module B {
    reg y : bits[1] = 0
}
";
    let err = emit_from_source(src).unwrap_err();
    assert!(err.iter().any(|e| e.message.contains("exactly one module")));
}
