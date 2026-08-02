use trace::types::{TypeError, Types, check};
use trace::{ast::Ast, lexer, parser, resolve};

fn run(src: &str) -> (Ast, Types, Vec<TypeError>) {
    let (tokens, lex_errors) = lexer::lex(src);
    assert!(lex_errors.is_empty(), "lex errors: {lex_errors:?}");
    let (ast, parse_errors) = parser::parse(src, &tokens);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let (res, resolve_errors) = resolve::resolve(&ast);
    assert!(
        resolve_errors.is_empty(),
        "resolve errors: {resolve_errors:?}"
    );
    let (types, errors) = check(&ast, &res);
    (ast, types, errors)
}

fn run_ok(src: &str) -> (Ast, Types) {
    let (ast, types, errors) = run(src);
    assert!(errors.is_empty(), "type errors: {errors:?}");
    (ast, types)
}

#[test]
fn modular_add_keeps_register_width() {
    // The doc's own SUBLEQ idiom must type: pc := pc + 3 into bits[16].
    run_ok("module M {\n reg pc : bits[16] = 0\n rule r {\n pc := pc + 3\n }\n}\n");
}

#[test]
fn wider_write_needs_trunc() {
    let src = "\
module M {
    reg a : bits[8] = 0
    reg b : bits[16] = 0

    rule r {
        a := b
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("trunc(value, 8)"));

    // And trunc fixes it.
    run_ok(
        "module M {\n reg a : bits[8] = 0\n reg b : bits[16] = 0\n rule r {\n a := trunc(b, 8)\n }\n}\n",
    );
}

#[test]
fn mul_sums_widths() {
    let src = "\
module M {
    reg a : bits[8] = 0
    reg p : bits[16] = 0

    rule r {
        p := a * a
    }
}
";
    run_ok(src);

    // bits[8] * bits[8] = bits[16] does not fit bits[15].
    let src = "\
module M {
    reg a : bits[8] = 0
    reg p : bits[15] = 0

    rule r {
        p := a * a
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("bits[16]"));
}

#[test]
fn arith_shift_keeps_the_left_operands_width_like_shr() {
    // `>>>` must NOT fall into the generic "mixed bits operands" rule
    // (max of both widths) the way an unlisted BinOp would -- it keeps
    // `x`'s own width exactly like `>>`/`<<` do, regardless of the
    // shift-amount operand's width.
    run_ok(
        "module M {\n reg x : bits[8] = 0\n reg n : bits[3] = 0\n reg y : bits[8] = 0\n \
         rule r {\n y := x >>> n\n }\n}\n",
    );
    let src = "\
module M {
    reg x : bits[8] = 0
    reg n : bits[3] = 0
    reg y : bits[7] = 0

    rule r {
        y := x >>> n
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("bits[8]"), "{errors:?}");
}

#[test]
fn fifo_element_types_check() {
    let src = "\
module M {
    fifo narrow : bits[8]
    fifo wide : bits[16]

    rule r {
        x := wide.Deq[]
        narrow.Enq[x]
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("enqueue"));

    run_ok(
        "module M {\n fifo a : bits[8]\n fifo b : bits[8]\n rule r {\n x := a.Deq[]\n b.Enq[x]\n }\n}\n",
    );
}

#[test]
fn mem_reads_give_element_type() {
    // m[pc] : bits[16]; writing it to an bits[8] reg must fail.
    let src = "\
module M {
    reg small : bits[8] = 0
    mem m : bits[16][256]
    reg pc : bits[8] = 0

    rule r {
        small := m[pc]
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("truncate"));
}

#[test]
fn bit_select_and_slice() {
    // x[0] is bits[1]; x[7..0] is bits[8].
    run_ok("F(x : bits[8]) : bits[1] <combines> {\n return x[0] ^ x[7]\n}\n");
    run_ok("G(x : bits[16]) : bits[8] <combines> {\n return x[7..0]\n}\n");

    let (_, _, errors) = run("H(x : bits[16]) : bits[4] <combines> {\n return x[7..0]\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("bits[8]"));
}

#[test]
fn dynamic_slice_bounds_are_a_type_error() {
    // Unlike a single index (always exactly 1 bit, static or dynamic), a
    // slice's WIDTH depends on both bounds -- if either isn't a
    // compile-time constant, the width genuinely can't be known, so
    // this must be an explicit type error, not silently fall through to
    // some default width (it used to silently type as bits[1], a real
    // latent mistyping bug: a narrower-than-declared value would have
    // passed `check_assignable` without complaint).
    let src = "\
module M {
    in x : bits[8]
    in a : bits[3]
    in b : bits[3]
    out y : bits[8] = 0
    rule r {
        y := x[a..b]
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("must both be compile-time constants")
    );

    // A single dynamic index is unaffected -- still well-typed as
    // bits[1], same as a static one.
    run_ok(
        "module M {\n in x : bits[8]\n in i : bits[3]\n out y : bits[1] = 0\n \
         rule r {\n y := x[i]\n }\n}\n",
    );
}

#[test]
fn indexed_part_select_types_by_its_static_width_regardless_of_a_dynamic_base() {
    // `x[base +: width]`/`x[base -: width]`: `base` may be dynamic, but
    // `width` must be a compile-time constant -- the whole point is that
    // the RESULT's width is fixed even though the start position isn't
    // known until runtime.
    run_ok(
        "module M {\n in x : bits[8]\n in base : bits[3]\n out y : bits[4] = 0\n \
         rule r {\n y := x[base +: 4]\n }\n}\n",
    );
    run_ok(
        "module M {\n in x : bits[8]\n in base : bits[3]\n out y : bits[4] = 0\n \
         rule r {\n y := x[base -: 4]\n }\n}\n",
    );

    let src = "\
module M {
    in x : bits[8]
    in base : bits[3]
    in w : bits[3]
    out y : bits[4] = 0
    rule r {
        y := x[base +: w]
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("needs a compile-time constant width")
    );
}

#[test]
fn implicit_width_params_instantiate_at_call_sites() {
    // N solves to 8, so clog2(N) = 3: assigning to bits[3] works and
    // assigning to bits[2] fails.
    let src = "\
Enc(reqs : bits[N]) : bits[clog2(N)] <combines> {
    return prio(reqs)
}

module M {
    reg r : bits[8] = 0
    reg g : bits[3] = 0

    rule pick {
        g := Enc(r)
    }
}
";
    run_ok(src);

    let src = "\
Enc(reqs : bits[N]) : bits[clog2(N)] <combines> {
    return prio(reqs)
}

module M {
    reg r : bits[8] = 0
    reg g : bits[2] = 0

    rule pick {
        g := Enc(r)
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("bits[3]"), "{errors:?}");
}

#[test]
fn arg_count_checked() {
    let src = "\
F(x : bits[8]) : bits[8] <combines> {
    return x
}

rule r {
    y := F(1, 2)
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("takes 1 argument(s), got 2"));
}

#[test]
fn conditions_must_be_one_bit() {
    let src = "\
module M {
    reg a : bits[8] = 0
    reg b : bits[8] = 0

    rule r {
        if a { b := 1 }
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("bits[1]"));

    run_ok(
        "module M {\n reg a : bits[8] = 0\n reg b : bits[8] = 0\n rule r {\n if a <> 0 { b := 1 }\n }\n}\n",
    );
}

#[test]
fn a_bare_condition_implicitly_guards_and_must_be_bits_1() {
    // `a = 1` alone (no `?`, value unused) now means the same thing
    // as `(a = 1)?` -- including the bits[1] enforcement `if`/`while`
    // conditions already get.
    run_ok(
        "module M {\n reg a : bits[8] = 0\n reg b : bits[8] = 0\n rule r {\n a = 1\n b := 1\n }\n}\n",
    );

    // A bare non-bits[1] expression (its value computed and left
    // unused) is now a type error instead of silently compiling to
    // dead code.
    let src = "\
module M {
    reg a : bits[8] = 0
    reg b : bits[8] = 0

    rule r {
        a
        b := 1
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("bits[1]"));
}

#[test]
fn a_bare_mem_read_wider_than_one_bit_is_a_clean_type_error() {
    // A mem read (`m[addr]`) is `Expr::Bracket`, the SAME AST shape a
    // fifo op and a bit-select both use -- sitting bare, it's neither
    // (not a fifo: `m` isn't a fifo def), so it's guard-like and must
    // be bits[1]. Its element type here is bits[8], so this must be a
    // clean type error, not a panic (e.g. in firrtl's read-port
    // collection, which walks bare statements too).
    let src = "\
module M {
    mem m : bits[8][256]
    reg pc : bits[8] = 0
    reg b : bits[8] = 0

    rule r {
        m[pc]
        b := 1
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("bits[1]"));
}

#[test]
fn an_explicit_guards_inner_expression_must_also_be_bits_1() {
    // Closes a latent gap: `expr?`'s inner expression was previously
    // never bits[1]-checked at all (passthrough typing). It now gets
    // exactly the same enforcement the new implicit-guard case does.
    let src = "\
module M {
    reg a : bits[8] = 0
    reg b : bits[8] = 0

    rule r {
        a?
        b := 1
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("bits[1]"));
}

#[test]
fn logical_not_needs_a_bits_1_operand() {
    // `not` and `~` compile to the identical FIRRTL `not` primop (see
    // firrtl/expr.rs) -- what makes `not` a real, distinct operator rather
    // than pure aliasing is this restriction: unlike `~`, which accepts
    // any width, `not` requires its operand already be `bits[1]`, since
    // there's no implicit "nonzero is true" coercion anywhere in this
    // language (`conditions_must_be_one_bit`, above) for a wider `not x`
    // to usefully mean.
    let src = "\
module M {
    reg a : bits[8] = 0
    out b : bits[8] = 0

    rule r {
        b := not a
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("`not` needs a bits[1] operand"));

    // A genuine bits[1] value -- a comparison's own result -- is fine.
    run_ok(
        "module M {\n reg a : bits[8] = 0\n out b : bits[1] = 0\n rule r {\n b := not (a = 0)\n }\n}\n",
    );
}

#[test]
fn shape_errors() {
    // Indexing a register.
    let (_, _, errors) = run("module M {\n reg a : bits[8] = 0\n rule r {\n x := a(3)\n }\n}\n");
    assert!(!errors.is_empty());

    // Arithmetic on a fifo.
    let (_, _, errors) =
        run("module M {\n fifo f : bits[8]\n reg a : bits[8] = 0\n rule r {\n a := f + 1\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("fifo"));
}

#[test]
fn rules_are_not_callable() {
    let src = "\
module M {
    reg a : bits[8] = 0
    rule t {
        a := a + 1
    }
    rule r {
        x := t()
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("not callable"));
}

#[test]
fn generic_bodies_skip_width_checks() {
    // Inside a generic fn, widths are unknown: no false errors.
    run_ok("Mix(a : bits[N], b : bits[N]) : bits[N] <combines> {\n return (a & b) ^ (a | b)\n}\n");
}

#[test]
fn literals_must_fit() {
    let (_, _, errors) = run("module M {\n reg a : bits[4] = 300\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("300 does not fit in bits[4]"));

    let (_, _, errors) = run("module M {\n reg a : bits[4] = 0\n rule r {\n a := 16\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("16 does not fit"));

    run_ok("module M {\n reg a : bits[4] = 15\n}\n");
}

#[test]
fn sized_literals_type_directly_and_check_their_own_width() {
    // Unlike a bare literal, `4'd20` has a definite width of its own —
    // checked immediately against ITS OWN declared width, not deferred
    // to wherever it's later used.
    let (_, _, errors) = run("module M {\n out a : bits[8] = 0\n rule r {\n a := 4'd20\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("20 does not fit in bits[4]"));

    // A real Bits type, not the coercible Ty::Int a bare literal gets:
    // combining it with a wider value widens to the max, the same rule
    // two differently-sized real registers already get, not an error.
    run_ok(
        "module M {\n in x : bits[16]\n out result : bits[16] = 0\n rule r {\n \
         result := x + 8'd6\n }\n}\n",
    );
}

#[test]
fn reg_and_output_infer_type_from_a_sized_literal_init() {
    // `reg a = 8'd6` behaves identically to `reg a : bits[8] = 8'd6` from
    // types.rs's perspective onward -- a later out-of-range write against
    // the INFERRED width is still an error, proving the synthesized type
    // is a real bits[8], not just accepted syntax with no teeth.
    let (_, _, errors) = run("module M {\n reg a = 8'd6\n rule r {\n a := 300\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("300 does not fit in bits[8]"));

    run_ok("module M {\n reg a = 8'd6\n out b = 16'hFF00\n}\n");
}

#[test]
fn instance_ports_check_direction_and_width() {
    let child = "module Child {\n in a : bits[8]\n out b : bits[8] = 0\n \
                  rule r {\n b := a\n}\n}\n";

    // Writing an input, reading an output: fine.
    run_ok(&format!(
        "{child}module Top {{\n inst c : Child\n reg v : bits[8] = 0\n \
         rule w {{\n c.a := v\n v := c.b\n}}\n}}\n"
    ));

    // Writing an output port is backwards.
    let (_, _, errors) = run(&format!(
        "{child}module Top {{\n inst c : Child\n rule w {{\n c.b := 1\n}}\n}}\n"
    ));
    assert!(errors.iter().any(|e| e.message.contains("output port")));

    // Reading an input port is backwards.
    let (_, _, errors) = run(&format!(
        "{child}module Top {{\n inst c : Child\n reg v : bits[8] = 0\n \
         rule w {{\n v := c.a\n}}\n}}\n"
    ));
    assert!(errors.iter().any(|e| e.message.contains("input port")));

    // No such port.
    let (_, _, errors) = run(&format!(
        "{child}module Top {{\n inst c : Child\n reg v : bits[8] = 0\n \
         rule w {{\n v := c.nope\n}}\n}}\n"
    ));
    assert!(errors.iter().any(|e| e.message.contains("no port")));

    // A wider value into a narrower input port needs `trunc`, same as any
    // other state write.
    let (_, _, errors) = run(&format!(
        "{child}module Top {{\n inst c : Child\n reg v : bits[16] = 0\n \
         rule w {{\n c.a := v\n}}\n}}\n"
    ));
    assert!(errors.iter().any(|e| e.message.contains("trunc")));
}

#[test]
fn spawn_types_a_handle_whose_result_and_done_fields_are_readable() {
    let src = "\
Slow(x : bits[8]) : bits[8] <sequences> {
    tick
    return x
}

module M {
    out out : bits[8] = 0

    rule r <sequences> {
        h := spawn Slow(1)
        tick
        (h.done = 1)?
        out := h.result
    }
}
";
    run_ok(src);
}

#[test]
fn handle_field_write_is_rejected() {
    let src = "\
Slow(x : bits[8]) : bits[8] <sequences> {
    tick
    return x
}

module M {
    rule r <sequences> {
        h := spawn Slow(1)
        tick
        h.result := 1
    }
}
";
    let (_, _, errors) = run(src);
    assert!(errors.iter().any(|e| e.message.contains("read-only")));
}

#[test]
fn handle_has_no_field_besides_result_and_done() {
    let src = "\
Slow(x : bits[8]) : bits[8] <sequences> {
    tick
    return x
}

module M {
    out out : bits[8] = 0

    rule r <sequences> {
        h := spawn Slow(1)
        tick
        out := h.nope
    }
}
";
    let (_, _, errors) = run(src);
    assert!(errors.iter().any(|e| e.message.contains("no field")));
}

#[test]
fn list_literal_types_as_list_of_its_element_type() {
    run_ok(
        "Sum(xs : list[bits[8]]) : bits[8] <elaborates> {\n\
             return xs[0]\n\
         }\n\
         module M {\n\
             in a : bits[8]\n\
             out r : bits[8] = 0\n\
             rule go {\n\
                 r := Sum([a])\n\
             }\n\
         }\n",
    );
}

#[test]
fn list_literal_element_type_mismatch_is_an_error() {
    let src = "Sum(xs : list[bits[8]]) : bits[8] <elaborates> {\n\
                   return xs[0]\n\
               }\n\
               module M {\n\
                   in a : bits[8]\n\
                   in w : bits[16]\n\
                   out r : bits[8] = 0\n\
                   rule go {\n\
                       r := Sum([a, w])\n\
                   }\n\
               }\n";
    let (_, _, errors) = run(src);
    assert!(
        !errors.is_empty(),
        "expected a type error for mismatched list elements"
    );
}

#[test]
fn an_empty_list_literal_is_an_error() {
    let src = "module M {\n\
                   out r : bits[8] = 0\n\
                   rule go {\n\
                       let xs = []\n\
                   }\n\
               }\n";
    let (_, _, errors) = run(src);
    assert!(
        errors.iter().any(|e| e.message.contains("empty")),
        "expected an empty-list-literal error, got: {errors:?}"
    );
}

#[test]
fn all_examples_type_check() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples");
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "tr") {
            let src = std::fs::read_to_string(&path).unwrap();
            let (_, _, errors) = run(&src);
            assert!(errors.is_empty(), "type errors in {path:?}: {errors:?}");
        }
    }
}
