use trace::types::{TypeError, Types, check};
use trace::{ast::Ast, effects, lexer, parser, resolve};

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
    // Not asserted empty here (unlike `emit_from_source`, tests/firrtl.rs):
    // this helper is for TYPE-level tests specifically, several of which
    // exercise a rule/fn shape that's type-valid but not necessarily
    // effect-valid (e.g. a bare `while` with no `<sequences>`/`<elaborates>`
    // tag) -- `fx` is only threaded through so `Stmt::IfLet`'s `is_failing_
    // call` check has signatures to look at, not to gate this helper's own
    // success on effect-checking passing too.
    let (fx, _effect_errors) = effects::check(&ast, &res);
    let (types, errors) = check(&ast, &res, &fx);
    (ast, types, errors)
}

fn run_ok(src: &str) -> (Ast, Types) {
    let (ast, types, errors) = run(src);
    assert!(errors.is_empty(), "type errors: {errors:?}");
    (ast, types)
}

#[test]
fn modular_add_keeps_register_width() {
    // The doc's own SUBLEQ idiom must type: pc := pc + 3 into [16].
    run_ok("module M {\n reg pc : [16] = 0\n rule r {\n pc := pc + 3\n }\n}\n");
}

#[test]
fn wider_write_needs_trunc() {
    let src = "\
module M {
    reg a : [8] = 0
    reg b : [16] = 0

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
        "module M {\n reg a : [8] = 0\n reg b : [16] = 0\n rule r {\n a := trunc(b, 8)\n }\n}\n",
    );
}

#[test]
fn zext_and_sext_reject_a_target_width_narrower_than_the_value() {
    for name in ["zext", "sext"] {
        let src = format!(
            "module M {{\n in a : [16]\n out result : [8] = 0\n rule r {{\n result := \
             {name}(a, 8)\n }}\n}}\n"
        );
        let (_, _, errors) = run(&src);
        assert_eq!(errors.len(), 1, "{name}: {errors:?}");
        assert!(errors[0].message.contains("use `trunc` to narrow instead"));
    }

    // A target width at least as wide as the value is accepted.
    run_ok(
        "module M {\n in a : [8]\n out result : [16] = 0\n rule r {\n result := zext(a, 16)\n }\n}\n",
    );
    run_ok(
        "module M {\n in a : [8]\n out result : [16] = 0\n rule r {\n result := sext(a, 16)\n }\n}\n",
    );
}

#[test]
fn mux_rejects_a_selector_wider_than_one_bit() {
    let src = "\
module M {
    in sel : [2]
    in a : [8]
    in b : [8]
    out result : [8] = 0
    rule r {
        result := mux(sel, a, b)
    }
}
";
    let (_, _, errors) = run(src);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("must be `bits[1]`"))
    );

    // A bits[1] selector is accepted, and the result width is the max
    // of the two arms.
    run_ok(
        "module M {\n in sel : [1]\n in a : [8]\n in b : [8]\n out result : [8] = 0\n rule r \
         {\n result := mux(sel, a, b)\n }\n}\n",
    );
}

/// Arity is validated in VALUE position (`type_builtin_call`, reached by
/// an ordinary `x := max(...)` rule-body expression), not in the
/// type-position width-expression path (`[max(...)]`, `types/eval.rs`'s
/// `const_eval_expr`) -- that pure evaluator has no error-reporting side
/// channel at all and silently resolves to `Width::Unknown` on ANY
/// unrecognized shape, the same pre-existing behavior `clog2`'s own
/// misuse already has there (no test relies on THAT producing a message,
/// so this doesn't newly break it, just doesn't extend past it).
#[test]
fn max_and_min_require_at_least_two_arguments() {
    for name in ["max", "min"] {
        let src = format!(
            "module M {{\n out result : [8] = 0\n rule ru {{\n result := {name}(4)\n }}\n}}\n"
        );
        let (_, _, errors) = run(&src);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("at least two arguments")),
            "{name}: {errors:?}"
        );
    }
}

#[test]
fn max_and_min_fold_directly_in_a_type_position_width_expression() {
    // Not just an argument to another generic instantiation (see
    // `implicit_width_params_instantiate_at_call_sites`, just above, for
    // that shape with `clog2`) -- `max`/`min` also fold as a bare
    // constant width expression, no implicit params involved at all.
    run_ok("module M {\n reg r : [max(4, 8)] = 0\n rule ru {\n r := r\n }\n}\n");
    run_ok("module M {\n reg r : [min(4, 8)] = 0\n rule ru {\n r := r\n }\n}\n");

    let src = "\
module M {
    reg r : [max(4, 8)] = 0
    reg g : [7] = 0
    rule ru {
        g := r
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].message.contains("trunc"));
}

#[test]
fn max_and_min_instantiate_at_call_sites_matching_two_implicit_widths() {
    // Mirrors `implicit_width_params_instantiate_at_call_sites`'s own
    // `clog2` shape: `n`/`m` solve from the call site's own argument
    // widths (4 and 8), so `max(n, m)` resolves to 8 and `min(n, m)`
    // resolves to 4.
    let src = "\
Wider(x : [n], y : [m]) : [max(n, m)] <combines> {
    return zext(x, max(n, m))
}

module M {
    in a : [4]
    in b : [8]
    reg w : [8] = 0

    rule r {
        w := Wider(a, b)
    }
}
";
    run_ok(src);

    let src_narrow = "\
Wider(x : [n], y : [m]) : [max(n, m)] <combines> {
    return zext(x, max(n, m))
}

module M {
    in a : [4]
    in b : [8]
    reg w : [7] = 0

    rule r {
        w := Wider(a, b)
    }
}
";
    let (_, _, errors) = run(src_narrow);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].message.contains("trunc"));
}

#[test]
fn max_of_bits_values_types_as_the_max_of_their_widths() {
    // Not `Ty::Int` (compile-time constant folding) at all once a real
    // `Bits` argument is present -- this is the dual-purpose half of
    // `max`/`min`, distinct from `max_and_min_fold_directly_in_a_type_
    // position_width_expression`'s all-`Int` case above. `max`'s width is
    // the MAX of its two Bits arguments' own widths (mirroring `mux`'s
    // own rule) -- `max(a, b)` can need every bit either operand has,
    // regardless of which one happens to be numerically larger at
    // runtime. `min`'s own, DIFFERENT rule is pinned separately, just
    // below (`min_of_bits_values_types_as_the_min_of_their_widths`) --
    // reusing `max`'s rule for `min` too was a real bug caught by
    // advisor review before shipping: `min(a, b)` can never exceed
    // either operand, so it only ever needs the NARROWER one's width.
    run_ok(
        "module M {\n in a : [4]\n in b : [8]\n out r : [8] = 0\n rule ru {\n r := max(a, b)\n }\n}\n",
    );

    let src = "\
module M {
    in a : [4]
    in b : [8]
    out r : [7] = 0
    rule ru {
        r := max(a, b)
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].message.contains("trunc"));
}

#[test]
fn min_of_bits_values_types_as_the_min_of_their_widths() {
    // `min(a, b) <= a` and `<= b` always, so it never needs more bits
    // than the NARROWER operand -- unlike `max`, just above. A `[4]`
    // target is legal here even though `b` is `[8]`; rejecting it (the
    // bug this test exists to pin) would wrongly reject a write that can
    // never actually overflow.
    run_ok(
        "module M {\n in a : [4]\n in b : [8]\n out r : [4] = 0\n rule ru {\n r := min(a, b)\n }\n}\n",
    );

    // A target one bit narrower than even the smaller operand is still
    // correctly rejected.
    let src = "\
module M {
    in a : [4]
    in b : [8]
    out r : [3] = 0
    rule ru {
        r := min(a, b)
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].message.contains("trunc"));
}

#[test]
fn max_absorbs_a_bare_int_literal_checked_to_fit_the_bits_sibling() {
    // Mirrors `type_binop`'s own `(Bits, Int)` absorption rule
    // (`check_literal_fits`), reused for `max`/`min`'s mixed-argument
    // case: a literal that fits is silently fine; one that doesn't is
    // the same "does not fit" diagnostic a plain `a + 300` (`a : [8]`)
    // already gets.
    run_ok("module M {\n in a : [8]\n out r : [8] = 0\n rule ru {\n r := max(a, 100)\n }\n}\n");

    let src = "\
module M {
    in a : [8]
    out r : [8] = 0
    rule ru {
        r := max(a, 1000)
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].message.contains("does not fit"));
}

#[test]
fn max_rejects_a_non_bits_non_int_argument() {
    let src = "\
Foo() <combines> {
}

module M {
    reg s : [8] = 0
    out r : [8] = 0
    rule ru {
        r := max(s, Foo())
    }
}
";
    let (_, _, errors) = run(src);
    assert!(errors.iter().any(|e| e.message.contains("needs `bits`")));
}

#[test]
fn mul_sums_widths() {
    let src = "\
module M {
    reg a : [8] = 0
    reg p : [16] = 0

    rule r {
        p := a * a
    }
}
";
    run_ok(src);

    // [8] * [8] = [16] does not fit [15].
    let src = "\
module M {
    reg a : [8] = 0
    reg p : [15] = 0

    rule r {
        p := a * a
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("[16]"));
}

#[test]
fn arith_shift_keeps_the_left_operands_width_like_shr() {
    // `>>>` must NOT fall into the generic "mixed bits operands" rule
    // (max of both widths) the way an unlisted BinOp would -- it keeps
    // `x`'s own width exactly like `>>`/`<<` do, regardless of the
    // shift-amount operand's width.
    run_ok(
        "module M {\n reg x : [8] = 0\n reg n : [3] = 0\n reg y : [8] = 0\n \
         rule r {\n y := x >>> n\n }\n}\n",
    );
    let src = "\
module M {
    reg x : [8] = 0
    reg n : [3] = 0
    reg y : [7] = 0

    rule r {
        y := x >>> n
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("[8]"), "{errors:?}");
}

#[test]
fn fifo_element_types_check() {
    let src = "\
module M {
    fifo narrow : [8]
    fifo wide : [16]

    rule r {
        let x = wide.Deq[]
        narrow.Enq[x]
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("enqueue"));

    run_ok(
        "module M {\n fifo a : [8]\n fifo b : [8]\n rule r {\n let x = a.Deq[]\n b.Enq[x]\n }\n}\n",
    );
}

/// `or` needs no dedicated "not `Enq`" check of its own (checks.rs's
/// `check_or_shape`): `Enq[x]` has no value of its own, so it types as
/// `unit` — this ordinary width-assignability check on `Or`'s
/// alternatives already rejects it before firrtl emission's own checks
/// ever run.
#[test]
fn or_alternative_rejects_enq_via_ordinary_type_mismatch() {
    let src = "\
module M {
    fifo a : [8]
    fifo b : [8]
    in x : [8]
    out result : [8] = 0
    rule r {
        result := a.Deq[] or b.Enq[x]
    }
}
";
    let (_, _, errors) = run(src);
    assert!(!errors.is_empty());
    assert!(errors.iter().any(|e| e.message.contains("or")));
}

#[test]
fn mem_reads_give_element_type() {
    // m[pc] : [16]; writing it to an [8] reg must fail.
    let src = "\
module M {
    reg small : [8] = 0
    mem m : [16][256]
    reg pc : [8] = 0

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
    // x[0] is [1]; x[7..0] is [8].
    run_ok("F(x : [8]) : [1] <combines> {\n return x[0] ^ x[7]\n}\n");
    run_ok("G(x : [16]) : [8] <combines> {\n return x[7..0]\n}\n");

    let (_, _, errors) = run("H(x : [16]) : [4] <combines> {\n return x[7..0]\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("[8]"));
}

#[test]
fn dynamic_slice_bounds_are_a_type_error() {
    // Unlike a single index (always exactly 1 bit, static or dynamic), a
    // slice's WIDTH depends on both bounds -- if either isn't a
    // compile-time constant, the width genuinely can't be known, so
    // this must be an explicit type error, not silently fall through to
    // some default width (it used to silently type as [1], a real
    // latent mistyping bug: a narrower-than-declared value would have
    // passed `check_assignable` without complaint).
    let src = "\
module M {
    in x : [8]
    in a : [3]
    in b : [3]
    out y : [8] = 0
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
    // [1], same as a static one.
    run_ok(
        "module M {\n in x : [8]\n in i : [3]\n out y : [1] = 0\n \
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
        "module M {\n in x : [8]\n in base : [3]\n out y : [4] = 0\n \
         rule r {\n y := x[base +: 4]\n }\n}\n",
    );
    run_ok(
        "module M {\n in x : [8]\n in base : [3]\n out y : [4] = 0\n \
         rule r {\n y := x[base -: 4]\n }\n}\n",
    );

    let src = "\
module M {
    in x : [8]
    in base : [3]
    in w : [3]
    out y : [4] = 0
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
    // N solves to 8, so clog2(N) = 3: assigning to [3] works and
    // assigning to [2] fails.
    let src = "\
Enc(reqs : [N]) : [clog2(N)] <combines> {
    return prio(reqs)
}

module M {
    reg r : [8] = 0
    reg g : [3] = 0

    rule pick {
        g := Enc(r)
    }
}
";
    run_ok(src);

    let src = "\
Enc(reqs : [N]) : [clog2(N)] <combines> {
    return prio(reqs)
}

module M {
    reg r : [8] = 0
    reg g : [2] = 0

    rule pick {
        g := Enc(r)
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("[3]"), "{errors:?}");
}

#[test]
fn arg_count_checked() {
    let src = "\
F(x : [8]) : [8] <combines> {
    return x
}

rule r {
    let y = F(1, 2)
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
    reg a : [8] = 0
    reg b : [8] = 0

    rule r {
        if a { b := 1 }
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("[1]"));

    run_ok(
        "module M {\n reg a : [8] = 0\n reg b : [8] = 0\n rule r {\n if a <> 0 { b := 1 }\n }\n}\n",
    );
}

#[test]
fn a_bare_condition_implicitly_guards_and_must_be_bits_1() {
    // `a = 1` alone (no `?`, value unused) now means the same thing
    // as `(a = 1)?` -- including the [1] enforcement `if`/`while`
    // conditions already get.
    run_ok("module M {\n reg a : [8] = 0\n reg b : [8] = 0\n rule r {\n a = 1\n b := 1\n }\n}\n");

    // A bare non-[1] expression (its value computed and left
    // unused) is now a type error instead of silently compiling to
    // dead code.
    let src = "\
module M {
    reg a : [8] = 0
    reg b : [8] = 0

    rule r {
        a
        b := 1
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("[1]"));
}

#[test]
fn a_bare_mem_read_wider_than_one_bit_is_a_clean_type_error() {
    // A mem read (`m[addr]`) is `Expr::Bracket`, the SAME AST shape a
    // fifo op and a bit-select both use -- sitting bare, it's neither
    // (not a fifo: `m` isn't a fifo def), so it's guard-like and must
    // be [1]. Its element type here is [8], so this must be a
    // clean type error, not a panic (e.g. in firrtl's read-port
    // collection, which walks bare statements too).
    let src = "\
module M {
    mem m : [8][256]
    reg pc : [8] = 0
    reg b : [8] = 0

    rule r {
        m[pc]
        b := 1
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("[1]"));
}

#[test]
fn an_explicit_guards_inner_expression_must_also_be_bits_1() {
    // Closes a latent gap: `expr?`'s inner expression was previously
    // never [1]-checked at all (passthrough typing). It now gets
    // exactly the same enforcement the new implicit-guard case does.
    let src = "\
module M {
    reg a : [8] = 0
    reg b : [8] = 0

    rule r {
        a?
        b := 1
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("[1]"));
}

#[test]
fn logical_not_needs_a_bits_1_operand() {
    // `not` and `~` compile to the identical FIRRTL `not` primop (see
    // firrtl/expr.rs) -- what makes `not` a real, distinct operator rather
    // than pure aliasing is this restriction: unlike `~`, which accepts
    // any width, `not` requires its operand already be `[1]`, since
    // there's no implicit "nonzero is true" coercion anywhere in this
    // language (`conditions_must_be_one_bit`, above) for a wider `not x`
    // to usefully mean.
    let src = "\
module M {
    reg a : [8] = 0
    out b : [8] = 0

    rule r {
        b := not a
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("`not` needs a [1] operand"));

    // A genuine [1] value -- `logic`'s discharge of a comparison -- is
    // fine (a bare comparison itself no longer types as [1] at all; see
    // TODO.md's comparisons-as-fallible design).
    run_ok(
        "module M {\n reg a : [8] = 0\n out b : [1] = 0\n rule r {\n b := not logic a = 0\n }\n}\n",
    );
}

#[test]
fn shape_errors() {
    // Indexing a register.
    let (_, _, errors) = run("module M {\n reg a : [8] = 0\n rule r {\n let x = a(3)\n }\n}\n");
    assert!(!errors.is_empty());

    // Arithmetic on a fifo.
    let (_, _, errors) =
        run("module M {\n fifo f : [8]\n reg a : [8] = 0\n rule r {\n a := f + 1\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("fifo"));
}

#[test]
fn generic_bodies_skip_width_checks() {
    // Inside a generic fn, widths are unknown: no false errors.
    run_ok("Mix(a : [N], b : [N]) : [N] <combines> {\n return (a & b) ^ (a | b)\n}\n");
}

#[test]
fn literals_must_fit() {
    let (_, _, errors) = run("module M {\n reg a : [4] = 300\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("300 does not fit in [4]"));

    let (_, _, errors) = run("module M {\n reg a : [4] = 0\n rule r {\n a := 16\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("16 does not fit"));

    run_ok("module M {\n reg a : [4] = 15\n}\n");
}

/// The bug this test is named for: `check_literal_fits` used to run
/// only at assignment-shaped coercion sites (a state write, a port
/// default, a destructure) — `type_binop`'s own `(Ty::Bits(w), Ty::Int)`
/// arm just absorbed the literal's `w` and moved on, so `a + 300` where
/// `a : [8]` silently typed as `[8]` with no diagnostic anywhere, not
/// even at emission. A call argument is exactly this shape (`Outer(a +
/// 100000000)`), which is how this was actually found: an edit to
/// examples/call_nested_writes.tr expected a compile error and got a
/// silently-accepted overflow instead.
#[test]
fn an_oversized_literal_combined_with_a_bits_value_via_a_binop_is_an_error() {
    let (_, _, errors) =
        run("module M {\n in a : [8]\n out b : [8] = 0\n rule r {\n b := a + 300\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("300 does not fit in [8]"));

    // Same check with the literal on the LEFT instead of the right —
    // `type_binop`'s two absorption arms are separate code paths now
    // (each checks a different child `ExprId`), so this exercises the
    // other one specifically, not just the same arm from the other side.
    let (_, _, errors) =
        run("module M {\n in a : [8]\n out b : [8] = 0\n rule r {\n b := 300 + a\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("300 does not fit in [8]"));

    // A comparison against an out-of-range literal gets the same check —
    // `a = 300` where `a : [8]` can never be true, the same class of
    // near-certainly-a-bug an over-wide state write already catches. A
    // bare comparison no longer types as [1] at all (TODO.md's
    // comparisons-as-fallible design; see the `if`-condition tests
    // below), so this is exercised as an `if` condition, its own
    // idiomatic use, not a direct assignment.
    let (_, _, errors) = run("module M {\n in a : [8]\n out b : [8] = 0\n rule r {\n \
         if a = 300 {\n b := 1\n }\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("300 does not fit in [8]"));

    // A literal that DOES fit is still fine, on either side.
    run_ok("module M {\n in a : [8]\n out b : [8] = 0\n rule r {\n b := a + 20\n }\n}\n");

    // A shift's second operand is a COUNT, not a value in the shifted
    // operand's own width domain, so `check_literal_fits` (a "does this
    // VALUE fit" check) is the wrong check for it — see
    // `shift_amounts_at_or_past_the_operand_width_are_an_error` below
    // for the shift-specific check this arm defers to instead.
    run_ok("module M {\n in a : [8]\n out b : [8] = 0\n rule r {\n b := a >> 3\n }\n}\n");
}

/// A constant shift amount `>= w` discards every bit of a `[w]` operand
/// — `>>`/`>>>` land on all-zero (or all-sign), `<<` shifts every
/// original bit out past the top (the result STAYS `[w]` wide; see
/// `arith_shift_keeps_the_left_operands_width_like_shr` above — it
/// doesn't grow to `[w + amount]`). Near-certainly a bug (a typo, a
/// swapped operand order), the same class `literals_must_fit` already
/// catches for ordinary arithmetic — just via a DIFFERENT bound
/// (`amount >= w`, not "needs more than `w` bits to represent"), since a
/// shift amount isn't itself a value bounded by the shifted operand's
/// width the way an arithmetic operand is (`literals_must_fit`'s own
/// last case exercises exactly that distinction from the other side).
#[test]
fn shift_amounts_at_or_past_the_operand_width_are_an_error() {
    // Right shift, bare (untyped) literal amount, way past the width.
    let (_, _, errors) =
        run("module M {\n in a : [8]\n out b : [8] = 0\n rule r {\n b := a >> 300\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("shift by 300 discards every bit of a [8] value")
    );

    // Exactly `w` is still a total discard (a `[8]` has bit indices
    // 0..7, so a shift by 8 has nothing left to land on) — off-by-one
    // from `w - 1`, the largest amount that keeps at least one bit,
    // deliberately checked as its own case here, not assumed from the
    // "way past" case above.
    let (_, _, errors) =
        run("module M {\n in a : [8]\n out b : [8] = 0\n rule r {\n b := a >> 8\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("shift by 8 discards every bit of a [8] value")
    );

    // A SIZED literal amount (a real Ty::Bits of its own, not the
    // coercible Ty::Int a bare literal gets — a different match arm in
    // type_binop entirely) is checked the same way.
    let (_, _, errors) =
        run("module M {\n in a : [8]\n out b : [8] = 0\n rule r {\n b := a >> 8'd8\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("shift by 8 discards every bit of a [8] value")
    );

    // Left shift and arithmetic-right-shift get the same treatment, not
    // just plain right shift.
    let (_, _, errors) =
        run("module M {\n in a : [8]\n out b : [8] = 0\n rule r {\n b := a << 8\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("discards every bit"));
    let (_, _, errors) =
        run("module M {\n in a : [8]\n out b : [8] = 0\n rule r {\n b := a >>> 8\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("discards every bit"));

    // One less than the width leaves exactly the sign/top bit behind —
    // the largest amount that ISN'T a total discard, so still accepted.
    run_ok("module M {\n in a : [8]\n out b : [8] = 0\n rule r {\n b := a >> 7\n }\n}\n");

    // A genuinely dynamic (non-constant) shift amount has nothing for
    // `const_eval` to evaluate, so it's silently skipped rather than
    // flagged — this check only ever fires against a compile-time
    // constant, the same restriction `check_literal_fits` itself has.
    run_ok(
        "module M {\n in a : [8]\n in n : [8]\n out b : [8] = 0\n rule r {\n \
         b := a >> n\n }\n}\n",
    );
}

/// `.!` (ast.rs's `lossy` set, populated by the parser right where it's
/// written — see tests/parser.rs) is an explicit, per-operator-
/// application opt-out of `check_literal_fits`/`check_shift_amount`
/// specifically — the two checks `shift_amounts_at_or_past_the_operand_
/// width_are_an_error` and `literals_must_fit` above cover. Every case
/// that errored there must accept its own `.!`-marked twin here.
#[test]
fn lossy_suffix_suppresses_the_literal_fits_and_shift_amount_checks() {
    // Ordinary arithmetic (`literals_must_fit`'s bug class).
    run_ok("module M {\n in a : [8]\n out b : [8] = 0\n rule r {\n b := a +.! 100000000\n }\n}\n");
    // The literal on the LEFT instead of the right — the OTHER absorption arm.
    run_ok("module M {\n in a : [8]\n out b : [8] = 0\n rule r {\n b := 300 +.! a\n }\n}\n");
    // A comparison against an out-of-range literal.
    run_ok(
        "module M {\n in a : [8]\n out b : [8] = 0\n rule r {\n \
         if a =.! 300 {\n b := 1\n }\n }\n}\n",
    );
    // A shift amount `>= w` (`shift_amounts_at_or_past_the_operand_
    // width_are_an_error`'s bug class) — bare literal, sized literal,
    // and a non-Shr shift op, each suppressed the same way.
    run_ok("module M {\n in a : [8]\n out b : [8] = 0\n rule r {\n b := a >>.! 300\n }\n}\n");
    run_ok("module M {\n in a : [8]\n out b : [8] = 0\n rule r {\n b := a >>.! 8'd8\n }\n}\n");
    run_ok("module M {\n in a : [8]\n out b : [8] = 0\n rule r {\n b := a <<.! 8\n }\n}\n");

    // `.!` marks THAT operator application specifically — an UNMARKED
    // sibling elsewhere in the same statement still errors normally, so
    // this isn't a blanket per-statement or per-rule suppression.
    let (_, _, errors) = run("module M {\n in a : [8]\n out b : [8] = 0\n rule r {\n \
         b := (a >>.! 300) + (a >> 300)\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("shift by 300"));

    // A deliberate asymmetry, both halves pinned here: `check_literal_
    // fits` self-gates on its own `value` argument (the generalization,
    // stmt.rs), so marking the LITERAL OPERAND directly also works, not
    // just the whole `Expr::Binary` — `a + 100000000.!` silences the
    // same check `a +.! 100000000` does. `check_shift_amount` does NOT
    // gain a matching self-gate (its only callers are already gated
    // externally by `type_binop`'s own `at`-keyed check), so the same
    // operand-marking spelling does NOT reach it: `a >> 300.!` still
    // errors — only `a >>.! 300`/`(a >> 300).!` (marking the whole
    // application) silences a shift's own check.
    run_ok("module M {\n in a : [8]\n out b : [8] = 0\n rule r {\n b := a + 100000000.!\n }\n}\n");
    let (_, _, errors) =
        run("module M {\n in a : [8]\n out b : [8] = 0\n rule r {\n b := a >> 300.!\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("discards every bit"));
}

/// `.!`'s generalization: a genuine postfix marker on ANY expression
/// (not just a binary operator's own mid-application spelling), also
/// silencing `check_assignable`'s write-position "would silently
/// truncate" check — previously out of `.!`'s scope entirely (DESIGN.md
/// used to say so explicitly).
#[test]
fn lossy_suffix_also_suppresses_the_write_position_assignability_check() {
    // Unmarked: the ordinary truncation error.
    let (_, _, errors) =
        run("module M {\n in a : [16]\n out b : [8] = 0\n rule r {\n b := a\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("would silently truncate"));

    // Marked directly on the write's RHS: silenced.
    run_ok("module M {\n in a : [16]\n out b : [8] = 0\n rule r {\n b := a.!\n }\n}\n");
}

/// The defining property of the general form: marking an OUTER node
/// does not reach into a DIFFERENT, unmarked node nested inside it —
/// "top-scope only." A call's own id and one of its arguments' ids are
/// two distinct `ExprId`s, so marking the call doesn't silence the
/// argument's own `check_assignable`, but marking the argument directly
/// does.
#[test]
fn lossy_suffix_does_not_reach_into_an_unmarked_argument() {
    let src = |lossy_call: bool, lossy_arg: bool| {
        format!(
            "Narrow(x : [8]) : [8] <combines> {{\n return x\n }}\n\
             module M {{\n in a : [16]\n out b : [8] = 0\n rule r {{\n \
             b := Narrow(a{}){}\n }}\n}}\n",
            if lossy_arg { ".!" } else { "" },
            if lossy_call { ".!" } else { "" },
        )
    };
    // Marking the CALL doesn't silence the argument's own check.
    let (_, _, errors) = run(&src(true, false));
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("argument would silently truncate")
    );

    // Marking the ARGUMENT itself does.
    run_ok(&src(false, true));
}

/// The mid-operator spelling and the general postfix spelling mark the
/// SAME id — so one mark can satisfy two DIFFERENT checks at once, when
/// that id happens to be anchor for both. `a +.! b`, written as an
/// entire write's RHS, silences BOTH the addition's own absorption
/// check AND the write's own `check_assignable` truncation check,
/// since the `Expr::Binary` built IS the write's whole RHS — not a
/// special case, just the same id satisfying two checks it happens to
/// be the anchor for. Nesting it breaks the coincidence: wrapped inside
/// a larger, unmarked expression, only the inner check is silenced.
#[test]
fn lossy_mid_operator_mark_also_reaches_the_write_check_when_it_is_the_whole_rhs() {
    // The addition IS the entire write's RHS: one mark, two checks.
    run_ok(
        "module M {\n in a : [8]\n in b : [8]\n out x : [4] = 0\n rule r {\n \
             x := a +.! b\n }\n}\n",
    );

    // Nested inside a larger, unmarked sum: only the inner check is
    // silenced — the write's own anchor is the OUTER `+`, unmarked.
    let (_, _, errors) = run(
        "module M {\n in a : [8]\n in b : [8]\n in c : [8]\n out x : [4] = 0\n rule r {\n \
         x := (a +.! b) + c\n }\n}\n",
    );
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("would silently truncate"));

    // Marking the outer sum too silences the write's own check as well.
    run_ok(
        "module M {\n in a : [8]\n in b : [8]\n in c : [8]\n out x : [4] = 0\n rule r {\n \
         x := ((a +.! b) + c).!\n }\n}\n",
    );
}

/// Recursion threading: `out : ?[4] := wide.!` peels the TARGET's
/// `Option` layer (same expression, same id — `check_assignable`'s
/// `(_, Ty::Option(inner))` arm) before reaching the `Bits`-vs-`Bits`
/// truncation arm, so the mark on `wide` must survive that peel.
#[test]
fn lossy_suffix_survives_the_implicit_option_wrap_recursion() {
    let (_, _, errors) = run(
        "module M {\n in a : [16]\n out b : ?[4] = false\n rule r {\n \
             b := optional a\n }\n}\n",
    );
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("would silently truncate"));

    run_ok(
        "module M {\n in a : [16]\n out b : ?[4] = false\n rule r {\n \
         b := optional a.!\n }\n}\n",
    );
}

#[test]
fn sized_literals_type_directly_and_check_their_own_width() {
    // Unlike a bare literal, `4'd20` has a definite width of its own —
    // checked immediately against ITS OWN declared width, not deferred
    // to wherever it's later used.
    let (_, _, errors) = run("module M {\n out a : [8] = 0\n rule r {\n a := 4'd20\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("20 does not fit in [4]"));

    // A real Bits type, not the coercible Ty::Int a bare literal gets:
    // combining it with a wider value widens to the max, the same rule
    // two differently-sized real registers already get, not an error.
    run_ok(
        "module M {\n in x : [16]\n out result : [16] = 0\n rule r {\n \
         result := x + 8'd6\n }\n}\n",
    );
}

#[test]
fn reg_and_output_infer_type_from_a_sized_literal_init() {
    // `reg a = 8'd6` behaves identically to `reg a : [8] = 8'd6` from
    // types.rs's perspective onward -- a later out-of-range write against
    // the INFERRED width is still an error, proving the synthesized type
    // is a real [8], not just accepted syntax with no teeth.
    let (_, _, errors) = run("module M {\n reg a = 8'd6\n rule r {\n a := 300\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("300 does not fit in [8]"));

    run_ok("module M {\n reg a = 8'd6\n out b = 16'hFF00\n}\n");
}

#[test]
fn instance_ports_check_direction_and_width() {
    let child = "module Child {\n in a : [8]\n out b : [8] = 0\n \
                  rule r {\n b := a\n}\n}\n";

    // Writing an input, reading an output: fine.
    run_ok(&format!(
        "{child}module Top {{\n inst c : Child\n reg v : [8] = 0\n \
         rule w {{\n c.a := v\n v := c.b\n}}\n}}\n"
    ));

    // Writing an output port is backwards.
    let (_, _, errors) = run(&format!(
        "{child}module Top {{\n inst c : Child\n rule w {{\n c.b := 1\n}}\n}}\n"
    ));
    assert!(errors.iter().any(|e| e.message.contains("output port")));

    // Reading an input port is backwards.
    let (_, _, errors) = run(&format!(
        "{child}module Top {{\n inst c : Child\n reg v : [8] = 0\n \
         rule w {{\n v := c.a\n}}\n}}\n"
    ));
    assert!(errors.iter().any(|e| e.message.contains("input port")));

    // No such port.
    let (_, _, errors) = run(&format!(
        "{child}module Top {{\n inst c : Child\n reg v : [8] = 0\n \
         rule w {{\n v := c.nope\n}}\n}}\n"
    ));
    assert!(errors.iter().any(|e| e.message.contains("no port")));

    // A wider value into a narrower input port needs `trunc`, same as any
    // other state write.
    let (_, _, errors) = run(&format!(
        "{child}module Top {{\n inst c : Child\n reg v : [16] = 0\n \
         rule w {{\n c.a := v\n}}\n}}\n"
    ));
    assert!(errors.iter().any(|e| e.message.contains("trunc")));
}

const TRIBUF: &str = "extmodule TriBuf from \"tribuf.v\" {\n in enable : [1]\n \
     in data : [8]\n out sensed : [8]\n io pad : [8]\n}\n";

#[test]
fn extmodule_in_and_out_ports_are_writable_and_readable_via_inst_port() {
    run_ok(&format!(
        "{TRIBUF}module Top {{\n inst t : TriBuf\n reg v : [8] = 0\n \
         reg e : [1] = 0\n rule w {{\n t.enable := e\n t.data := v\n v := t.sensed\n}}\n}}\n"
    ));
}

#[test]
fn extmodule_io_port_cannot_be_read_or_written_via_inst_port() {
    let (_, _, errors) = run(&format!(
        "{TRIBUF}module Top {{\n inst t : TriBuf\n reg v : [8] = 0\n \
         rule w {{\n v := t.pad\n}}\n}}\n"
    ));
    assert!(errors.iter().any(|e| e.message.contains("io port")));

    let (_, _, errors) = run(&format!(
        "{TRIBUF}module Top {{\n inst t : TriBuf\n reg v : [8] = 0\n \
         rule w {{\n t.pad := v\n}}\n}}\n"
    ));
    assert!(errors.iter().any(|e| e.message.contains("io port")));
}

#[test]
fn extmodule_io_port_must_be_a_plain_bit_width() {
    let src = "\
struct Pair {
    x : [8]
    y : [8]
}

extmodule Bad from \"bad.v\" {
    io p : Pair
}
";
    let (_, _, errors) = run(src);
    assert!(errors.iter().any(|e| e.message.contains("plain bit width")));
}

#[test]
fn attach_wires_a_modules_own_io_port_to_an_extmodules_io_port() {
    run_ok(&format!(
        "{TRIBUF}module Top {{\n io bus : [8]\n inst t : TriBuf\n attach bus, t.pad\n}}\n"
    ));
}

#[test]
fn instance_io_ports_cannot_be_read_or_written_from_a_rule_body() {
    let child = "module Child {\n io bus : [8]\n}\n";

    let (_, _, errors) = run(&format!(
        "{child}module Top {{\n inst c : Child\n reg v : [8] = 0\n \
         rule w {{\n c.bus := v\n}}\n}}\n"
    ));
    assert!(errors.iter().any(|e| e.message.contains("io port")));

    let (_, _, errors) = run(&format!(
        "{child}module Top {{\n inst c : Child\n reg v : [8] = 0\n \
         rule w {{\n v := c.bus\n}}\n}}\n"
    ));
    assert!(errors.iter().any(|e| e.message.contains("io port")));
}

#[test]
fn attach_accepts_two_matching_width_io_ports() {
    run_ok("module M {\n io a : [8]\n io b : [8]\n attach a, b\n}\n");
    run_ok(
        "module Child {\n io bus : [8]\n}\n\
         module Top {\n io bus : [8]\n inst c : Child\n attach bus, c.bus\n}\n",
    );
}

#[test]
fn attach_rejects_mismatched_widths() {
    let (_, _, errors) = run("module M {\n io a : [8]\n io b : [16]\n attach a, b\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("same type"));
}

#[test]
fn attach_rejects_a_non_io_instance_port() {
    let child = "module Child {\n out b : [8] = 0\n}\n";
    let (_, _, errors) = run(&format!(
        "{child}module Top {{\n io a : [8]\n inst c : Child\n attach a, c.b\n}}\n"
    ));
    assert!(errors.iter().any(|e| e.message.contains("not an io port")));
}

#[test]
fn attach_rejects_a_sibling_modules_own_name_instead_of_an_instance() {
    // `A.bus` here names the MODULE `A`, not a bound `inst c : A` — a
    // shape that must be rejected here rather than reach emission (it
    // isn't `instance.port`, and letting it through would emit FIRRTL
    // referencing a declaration that doesn't exist in this module's own
    // scope; firtool would reject it with a confusing raw error instead
    // of trace giving a clean one -- self-caught by probing exactly this).
    let (_, _, errors) = run("module A {\n io bus : [8]\n}\n\
         module Top {\n io bus : [8]\n inst c : A\n attach bus, A.bus\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("instance.port"));
}

#[test]
fn attach_rejects_an_unknown_instance_port() {
    let child = "module Child {\n io bus : [8]\n}\n";
    let (_, _, errors) = run(&format!(
        "{child}module Top {{\n io a : [8]\n inst c : Child\n attach a, c.nope\n}}\n"
    ));
    assert!(errors.iter().any(|e| e.message.contains("no port")));
}

#[test]
fn io_port_holds_a_plain_bit_width_not_a_struct() {
    let src = "\
struct Pair {
    x : [8]
    y : [8]
}

module M {
    io p : Pair
}
";
    let (_, _, errors) = run(src);
    assert!(errors.iter().any(|e| e.message.contains("plain bit width")));
}

#[test]
fn spawn_types_a_handle_whose_result_and_done_fields_are_readable() {
    let src = "\
Slow(x : [8]) : [8] <sequences> {
    tick
    return x
}

module M {
    out out : [8] = 0

    rule r <sequences> {
        let h = spawn Slow(1)
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
Slow(x : [8]) : [8] <sequences> {
    tick
    return x
}

module M {
    rule r <sequences> {
        let h = spawn Slow(1)
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
Slow(x : [8]) : [8] <sequences> {
    tick
    return x
}

module M {
    out out : [8] = 0

    rule r <sequences> {
        let h = spawn Slow(1)
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
        "Sum(xs : list[8]) : [8] <elaborates> {\n\
             return xs[0]\n\
         }\n\
         module M {\n\
             in a : [8]\n\
             out r : [8] = 0\n\
             rule go {\n\
                 r := Sum([a])\n\
             }\n\
         }\n",
    );
}

#[test]
fn list_literal_element_type_mismatch_is_an_error() {
    let src = "Sum(xs : list[8]) : [8] <elaborates> {\n\
                   return xs[0]\n\
               }\n\
               module M {\n\
                   in a : [8]\n\
                   in w : [16]\n\
                   out r : [8] = 0\n\
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
fn list_of_a_named_struct_type_still_spells_the_element_type_out() {
    // `list[T]`'s bare-width sugar (`list[8]`, above) only kicks in when
    // the argument ISN'T already type-shaped (`eval_elem_ty`, types.rs) —
    // a struct name like `Pair` is, so `list[Pair]` keeps meaning a list
    // of that struct, not a list of `bits[Pair]` (which would be nonsense
    // anyway, since `Pair` doesn't resolve to a constant width).
    run_ok(
        "struct Pair {\n\
             valid : [1]\n\
             data : [8]\n\
         }\n\
         First(xs : list[Pair]) : Pair <elaborates> {\n\
             return xs[0]\n\
         }\n\
         module M {\n\
             reg p : Pair\n\
         }\n",
    );
}

#[test]
fn an_empty_list_literal_is_an_error() {
    let src = "module M {\n\
                   out r : [8] = 0\n\
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
fn struct_field_read_round_trips() {
    run_ok(
        "struct Pair {\n\
             valid : [1]\n\
             data : [8]\n\
         }\n\
         module M {\n\
             reg p : Pair = Pair{ valid: 0, data: 0 }\n\
             out ok : [1] = 0\n\
             rule r {\n\
                 p := Pair{ valid: 1, data: 8'd42 }\n\
                 ok := p.valid\n\
             }\n\
         }\n",
    );
}

#[test]
fn struct_field_write_is_rejected() {
    let src = "struct Pair {\n\
                   valid : [1]\n\
                   data : [8]\n\
               }\n\
               module M {\n\
                   reg p : Pair = Pair{ valid: 0, data: 0 }\n\
                   rule r {\n\
                       p.valid := 1\n\
                   }\n\
               }\n";
    let (_, _, errors) = run(src);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("read-only") && e.message.contains(".valid")),
        "expected a read-only-field error, got: {errors:?}"
    );
}

#[test]
fn struct_literal_missing_field_is_an_error() {
    let src = "struct Pair {\n\
                   valid : [1]\n\
                   data : [8]\n\
               }\n\
               module M {\n\
                   reg p : Pair = Pair{ valid: 0 }\n\
                   rule r {\n\
                       p?\n\
                   }\n\
               }\n";
    let (_, _, errors) = run(src);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("missing field") && e.message.contains("data")),
        "expected a missing-field error, got: {errors:?}"
    );
}

#[test]
fn struct_update_fills_missing_fields_from_base() {
    // `..old` supplies `valid`, left unnamed by this literal -- no
    // "missing field" error, unlike `struct_literal_missing_field_is_
    // an_error` above (same shape, no `..`).
    run_ok(
        "struct Pair {\n\
             valid : [1]\n\
             data : [8]\n\
         }\n\
         module M {\n\
             reg old : Pair = Pair{ valid: 1, data: 0 }\n\
             out result : [8] = 0\n\
             rule r {\n\
                 let p = Pair{ data: 5, ..old }\n\
                 result := p.data\n\
             }\n\
         }\n",
    );
}

#[test]
fn struct_update_base_must_match_the_struct_being_built() {
    let src = "struct Pair {\n\
                   valid : [1]\n\
                   data : [8]\n\
               }\n\
               struct Other {\n\
                   a : [1]\n\
                   b : [8]\n\
               }\n\
               module M {\n\
                   reg o : Other = Other{ a: 0, b: 0 }\n\
                   rule r {\n\
                       let p = Pair{ data: 5, ..o }\n\
                   }\n\
               }\n";
    let (_, _, errors) = run(src);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("`..` base") && e.message.contains("struct Pair")),
        "expected a base-type-mismatch rejection, got: {errors:?}"
    );
}

#[test]
fn struct_update_does_not_recurse_into_a_nested_partial_override() {
    // `..old`'s fill only ever reaches fields THIS literal doesn't name
    // -- `inner` IS named here (with its own, separately incomplete
    // literal), so its missing field (`y`) is NOT filled from `old.
    // inner.y`; it's an ordinary missing-field error on the NESTED
    // literal, checked independently of the outer `..old` (matching
    // Rust's own `..` semantics: never a recursive merge).
    let src = "struct Inner {\n\
                   x : [8]\n\
                   y : [8]\n\
               }\n\
               struct Outer {\n\
                   inner : Inner\n\
                   z : [1]\n\
               }\n\
               module M {\n\
                   reg old : Outer = Outer{ inner: Inner{ x: 0, y: 0 }, z: 0 }\n\
                   rule r {\n\
                       let o = Outer{ inner: Inner{ x: 9 }, ..old }\n\
                   }\n\
               }\n";
    let (_, _, errors) = run(src);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("missing field") && e.message.contains("y")),
        "expected the nested literal's own missing-field error, got: {errors:?}"
    );
}

#[test]
fn struct_update_is_rejected_in_a_reg_init() {
    // `base` isn't a compile-time constant in general (a reg reference's
    // flat fields aren't known until runtime), so `..` is rejected in a
    // reg/output init outright rather than silently defaulting the
    // fields it was meant to supply to 0 (`struct_lit_field_const`'s
    // `fields.iter().find(...)?` would otherwise return `None`, routed
    // by `module.rs` through `.unwrap_or(0)` with no error at all).
    let src = "struct Pair {\n\
                   valid : [1]\n\
                   data : [8]\n\
               }\n\
               module M {\n\
                   reg old : Pair = Pair{ valid: 1, data: 0 }\n\
                   reg p : Pair = Pair{ data: 5, ..old }\n\
               }\n";
    let (_, _, errors) = run(src);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("`..` isn't supported in a reg init")),
        "expected a const-init `..` rejection, got: {errors:?}"
    );
}

#[test]
fn struct_update_is_rejected_when_nested_inside_a_reg_init() {
    // The rejection is a WALK, not a top-level-only check: `..` shows up
    // one level deeper here (inside an explicitly-given field's own
    // nested literal), still reachable from the reg's init and still
    // not a compile-time constant.
    let src = "struct Inner {\n\
                   x : [8]\n\
                   y : [8]\n\
               }\n\
               struct Outer {\n\
                   inner : Inner\n\
                   z : [1]\n\
               }\n\
               module M {\n\
                   reg old_inner : Inner = Inner{ x: 1, y: 2 }\n\
                   reg o : Outer = Outer{ inner: Inner{ x: 9, ..old_inner }, z: 0 }\n\
               }\n";
    let (_, _, errors) = run(src);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("`..` isn't supported in a reg init")),
        "expected a const-init `..` rejection, got: {errors:?}"
    );
}

#[test]
fn struct_literal_extra_field_is_an_error() {
    let src = "struct Pair {\n\
                   valid : [1]\n\
                   data : [8]\n\
               }\n\
               module M {\n\
                   reg p : Pair = Pair{ valid: 0, data: 0, extra: 1 }\n\
                   rule r {\n\
                       p?\n\
                   }\n\
               }\n";
    let (_, _, errors) = run(src);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("has no field") && e.message.contains("extra")),
        "expected an unknown-field error, got: {errors:?}"
    );
}

#[test]
fn struct_literal_duplicate_field_is_an_error() {
    let src = "struct Pair {\n\
                   valid : [1]\n\
                   data : [8]\n\
               }\n\
               module M {\n\
                   reg p : Pair = Pair{ valid: 0, valid: 1, data: 0 }\n\
                   rule r {\n\
                       p?\n\
                   }\n\
               }\n";
    let (_, _, errors) = run(src);
    assert!(
        errors.iter().any(|e| e.message.contains("more than once")),
        "expected a duplicate-field error, got: {errors:?}"
    );
}

#[test]
fn nested_struct_field_type_checks_and_reads_by_chained_field() {
    run_ok(
        "struct Inner {\n\
             a : [1]\n\
             b : [8]\n\
         }\n\
         struct Outer {\n\
             inner : Inner\n\
             x : [1]\n\
         }\n\
         module M {\n\
             reg o : Outer = Outer{ inner: Inner{ a: 1, b: 8'd5 }, x: 0 }\n\
             out ok : [1] = 0\n\
             rule r {\n\
                 ok := o.inner.a\n\
             }\n\
         }\n",
    );
}

#[test]
fn self_referential_struct_is_rejected() {
    // Cycle detection runs over every declared struct regardless of
    // whether anything actually uses it -- no reg/rule needed to
    // trigger it.
    let src = "struct A {\n\
                   b : B\n\
               }\n\
               struct B {\n\
                   a : A\n\
               }\n\
               module M {\n\
               }\n";
    let (_, _, errors) = run(src);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("recursively defined")),
        "expected a self-referential-struct rejection, got: {errors:?}"
    );
}

#[test]
fn struct_to_struct_copy_is_rejected() {
    // `p := q` between two struct-typed regs isn't lowerable by
    // `struct_field_value_in_stmts` (it only decomposes a literal RHS
    // into per-field values) -- rejected here at the type-check level
    // instead of silently freezing `p` at its reset value in FIRRTL.
    let src = "struct Pair {\n\
                   valid : [1]\n\
                   data : [8]\n\
               }\n\
               module M {\n\
                   reg p : Pair = Pair{ valid: 0, data: 0 }\n\
                   reg q : Pair = Pair{ valid: 0, data: 0 }\n\
                   rule r {\n\
                       p := q\n\
                   }\n\
               }\n";
    let (_, _, errors) = run(src);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("must be a struct literal")),
        "expected a struct-copy rejection, got: {errors:?}"
    );
}

#[test]
fn option_construction_and_unwrap_type_check() {
    // `false` constructs absent; a bare `[8]` value coerces
    // implicitly to present; `opt?` unwraps to `[8]`; `.valid`/
    // `.data` read back `bit`/`[8]` directly.
    run_ok(
        "module M {\n\
             reg opt : ?[8] = false\n\
             out result : [8] = 0\n\
             out ok : [1] = 0\n\
             rule fill {\n\
                 opt := 8'd5\n\
             }\n\
             rule clear {\n\
                 opt := false\n\
             }\n\
             rule r {\n\
                 result := opt?\n\
                 ok := opt.valid\n\
             }\n\
         }\n",
    );
}

#[test]
fn nested_option_in_struct_field_type_checks() {
    run_ok(
        "struct Frame {\n\
             id : [4]\n\
             maybe : ?[8]\n\
         }\n\
         module M {\n\
             reg fr : Frame = Frame{ id: 0, maybe: false }\n\
             out ok : [1] = 0\n\
             rule r {\n\
                 ok := fr.maybe.valid\n\
             }\n\
         }\n",
    );
}

#[test]
fn let_destructure_binds_struct_fields_by_name() {
    // `let {valid, data: d} = p` desugars (parser.rs) into `let valid =
    // p.valid` + `let d = p.data` -- ordinary field-projection locals.
    // Naming every field of `p`'s type makes this exhaustive on its own
    // (no `..` needed) -- see `check_destructures`, types.rs.
    run_ok(
        "struct Pair {\n\
             valid : [1]\n\
             data : [8]\n\
         }\n\
         module M {\n\
             reg p : Pair = Pair{ valid: 1, data: 5 }\n\
             out result : [8] = 0\n\
             rule r {\n\
                 let {valid, data: d} = p\n\
                 result := d\n\
             }\n\
         }\n",
    );
}

#[test]
fn let_destructure_binds_option_valid_and_data() {
    run_ok(
        "module M {\n\
             reg opt : ?[8] = false\n\
             out result : [8] = 0\n\
             rule r {\n\
                 let {valid, data} = opt\n\
                 result := data\n\
             }\n\
         }\n",
    );
}

#[test]
fn let_destructure_two_options_needs_renaming_to_avoid_shadowing() {
    // `valid`/`data` are the two field names every `?T` has, so
    // destructuring two of them in one rule is the actual motivating
    // case for the rename form -- shorthand alone would silently shadow
    // the first pair (`let_shadowing_is_allowed`, tests/resolve.rs),
    // not error, so this pins the escape hatch actually works.
    run_ok(
        "module M {\n\
             reg p : ?[8] = false\n\
             reg q : ?[8] = false\n\
             out result : [8] = 0\n\
             rule r {\n\
                 let {valid: p_valid, data: p_data} = p\n\
                 let {valid: q_valid, data: q_data} = q\n\
                 result := p_data\n\
             }\n\
         }\n",
    );
}

#[test]
fn let_destructure_field_typo_reports_the_struct_has_no_such_field() {
    // A typo'd field name surfaces through the SAME error an equivalent
    // hand-written `let bind = source.field` would already give, because
    // that's literally what this desugars to. Also pins the double-error
    // trap `check_destructures` has to avoid: `vlaid` isn't a real field,
    // so exhaustiveness-checking against Pair's declared fields would
    // otherwise ALSO report "missing field(s): valid" alongside the typo
    // -- self-caught while implementing the exhaustiveness check itself.
    let src = "struct Pair {\n\
                   valid : [1]\n\
                   data : [8]\n\
               }\n\
               module M {\n\
                   reg p : Pair = Pair{ valid: 1, data: 5 }\n\
                   rule r {\n\
                       let {vlaid, data} = p\n\
                   }\n\
               }\n";
    let (_, _, errors) = run(src);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("has no field `vlaid`")),
        "expected a field-typo rejection, got: {errors:?}"
    );
    assert!(
        !errors.iter().any(|e| e.message.contains("missing field")),
        "typo'd field shouldn't ALSO trigger a missing-field error: {errors:?}"
    );
}

#[test]
fn let_destructure_non_exhaustive_without_rest_is_an_error() {
    // `p` has two fields (`valid`, `data`); naming only one without a
    // trailing `..` must error -- silently dropping `data` on the floor
    // is exactly what `..` exists to make an explicit, opt-in choice.
    let src = "struct Pair {\n\
                   valid : [1]\n\
                   data : [8]\n\
               }\n\
               module M {\n\
                   reg p : Pair = Pair{ valid: 1, data: 5 }\n\
                   rule r {\n\
                       let {valid} = p\n\
                   }\n\
               }\n";
    let (_, _, errors) = run(src);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("missing field(s): data")),
        "expected a missing-field rejection, got: {errors:?}"
    );
}

#[test]
fn let_destructure_with_rest_discards_remaining_fields() {
    // Same non-exhaustive pattern as above, but with a trailing `..` --
    // an explicit opt-in to discard `data`, so no error.
    run_ok(
        "struct Pair {\n\
             valid : [1]\n\
             data : [8]\n\
         }\n\
         module M {\n\
             reg p : Pair = Pair{ valid: 1, data: 5 }\n\
             out result : [1] = 0\n\
             rule r {\n\
                 let {valid, ..} = p\n\
                 result := valid\n\
             }\n\
         }\n",
    );
}

#[test]
fn let_destructure_option_non_exhaustive_without_rest_is_an_error() {
    // Same rule applies to `?T`'s two synthetic fields (`valid`/`data`),
    // not just user-declared structs.
    let src = "module M {\n\
                   reg opt : ?[8] = false\n\
                   rule r {\n\
                       let {valid} = opt\n\
                   }\n\
               }\n";
    let (_, _, errors) = run(src);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("missing field(s): data")),
        "expected a missing-field rejection, got: {errors:?}"
    );
}

#[test]
fn absent_literal_into_plain_bits_is_rejected() {
    // `false` isn't const-evaluable as an integer -- without routing
    // this through `check_assignable` too, `check_literal_fits` would
    // silently skip validating it entirely (see types.rs's
    // `collect_state` doc comment).
    let src = "module M {\n\
                   reg x : [8] = false\n\
                   rule r {\n\
                       x := 1\n\
                   }\n\
               }\n";
    let (_, _, errors) = run(src);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("expected [8]") && e.message.contains("false")),
        "expected a false-into-plain-bits rejection, got: {errors:?}"
    );
}

#[test]
fn optional_false_constructs_some_none_on_a_double_option() {
    // Bare coercion (`false`, a plain value) fills EVERY remaining `?`
    // layer at once, so `??[8]` can only reach fully-absent/fully-
    // present that way. `optional false` forces just the OUTER layer
    // present while the inner one stays absent -- `Some(None)`,
    // otherwise inexpressible (see DESIGN.md's "Option types" section).
    run_ok(
        "module M {\n\
             reg oo : ??[8] = optional false\n\
             out outer_valid : [1] = 0\n\
             out inner_valid : [1] = 0\n\
             rule r {\n\
                 outer_valid := oo.valid\n\
                 inner_valid := oo.data.valid\n\
             }\n\
         }\n",
    );
}

#[test]
fn optional_into_a_single_option_layer_has_no_inner_for_false_to_mean_anything() {
    // `optional`'s ONE layer and `?[8]`'s ONE layer already line up, so
    // there's no remaining Option layer left for `false` (which only
    // unifies against a `Ty::Option` target) to construct absence in --
    // a genuine type mismatch, not the `??T` case above.
    let src = "module M {\n\
                   reg opt : ?[8] = optional false\n\
               }\n";
    let (_, _, errors) = run(src);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("expected [8]") && e.message.contains("false")),
        "expected a mismatch inside `optional`'s single layer, got: {errors:?}"
    );
}

#[test]
fn optional_wrapping_an_existing_option_alias_is_rejected() {
    // `optional p`, `p` an EXISTING `?T`-typed reg (not a fresh literal
    // or computed value), would need emission to thread `p`'s own live
    // valid/data pair into another Option's flat fields -- unsupported,
    // the same "copying one `?T` value into another" v0 restriction
    // `option_to_option_copy_is_rejected` pins for a direct write, one
    // layer up. Caught here rather than left to silently miscompile at
    // emission (`compile_field_path_value`'s aliasing guard returns
    // `None` for this shape, but nothing on the WRITE side escalates a
    // `None` to a diagnostic -- self-caught by hand-probing before
    // considering the feature done).
    let src = "module M {\n\
                   reg p : ?[8] = false\n\
                   reg oo : ??[8] = false\n\
                   rule r {\n\
                       oo := optional p\n\
                   }\n\
               }\n";
    let (_, _, errors) = run(src);
    assert!(
        errors.iter().any(|e| e
            .message
            .contains("cannot wrap an existing `?T` value directly")),
        "expected an `optional`-alias rejection, got: {errors:?}"
    );
}

#[test]
fn optional_wrapping_a_call_returning_option_is_allowed() {
    // The one exception to the alias rejection above: a CALL returning
    // `?T` decomposes per-leaf (`compile_call_field_value`) rather than
    // aliasing a flat register, the same exemption `type_write`'s own
    // sibling Option-to-Option check already carves out for calls.
    run_ok(
        "Id(x : ?[8]) : ?[8] {\n\
             return x\n\
         }\n\
         module M {\n\
             reg oo : ??[8] = false\n\
             rule r {\n\
                 oo := optional Id(8'd5)\n\
             }\n\
         }\n",
    );
}

#[test]
fn optional_into_a_non_option_target_is_rejected() {
    // `optional e` has no standalone type of its own (mirrors `false`'s
    // `Ty::AbsentLit` sentinel) -- it only type-checks against a
    // `Ty::Option` target, which supplies the layer it doesn't know.
    // Also pins the reg-init routing fix: without diverting an
    // `Expr::Optional` init through `check_assignable` the same way a
    // literal `Expr::Absent` init already is, `check_literal_fits`
    // silently no-ops (its `const_eval` doesn't recognize `Expr::
    // Optional` either) and this passed with no error at all --
    // self-caught the same way the ORIGINAL `false`-into-`[8]` gap was.
    let src = "module M {\n\
                   reg x : [8] = optional 5\n\
               }\n";
    let (_, _, errors) = run(src);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("expected [8]") && e.message.contains("optional")),
        "expected an `optional`-outside-`?T` rejection, got: {errors:?}"
    );
}

#[test]
fn option_to_option_copy_is_rejected() {
    let src = "module M {\n\
                   reg p : ?[8] = false\n\
                   reg q : ?[8] = false\n\
                   rule r {\n\
                       p := q\n\
                   }\n\
               }\n";
    let (_, _, errors) = run(src);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("copying one `?T` value into another")),
        "expected an option-copy rejection, got: {errors:?}"
    );
}

#[test]
fn differently_shaped_option_write_is_one_plain_type_mismatch_not_two_errors() {
    // `??[8]` (state) vs `?[8]` (rhs) are both `Ty::Option`, but
    // NOT the same shape -- this must fall through to the ordinary
    // `check_assignable` type-mismatch error alone, not ALSO trip the
    // same-shape "copying one `?T` value into another" message (that
    // message is specifically about `p := q`, both `?[8]` -- wrong
    // and redundant here). Self-caught while probing `??T`: an earlier
    // version of the check fired on "both Option" instead of "same
    // Option", producing two errors for one mistake.
    let src = "module M {\n\
                   reg inner : ?[8] = false\n\
                   reg oo : ??[8] = false\n\
                   rule r {\n\
                       oo := inner\n\
                   }\n\
               }\n";
    let (_, _, errors) = run(src);
    assert_eq!(
        errors.len(),
        1,
        "expected exactly one error, got: {errors:?}"
    );
    assert!(errors[0].message.contains("expected ??[8], got ?[8]"));
}

#[test]
fn option_field_write_is_rejected() {
    let src = "module M {\n\
                   reg opt : ?[8] = false\n\
                   rule r {\n\
                       opt.valid := 1\n\
                   }\n\
               }\n";
    let (_, _, errors) = run(src);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("read-only") && e.message.contains(".valid")),
        "expected a read-only-field error, got: {errors:?}"
    );
}

#[test]
fn option_and_struct_fn_params_type_check() {
    // A struct/`?T`-typed fn param IS supported: both a literal
    // argument and a reg-typed argument (chased through by
    // `compile_struct_field_read`'s emission-side logic, not a type-
    // checking concern here) type-check fine. Returns are ALSO
    // supported now, see `option_and_struct_fn_returns_type_check`.
    run_ok(
        "struct Pair {\n\
             valid : [1]\n\
             data : [8]\n\
         }\n\
         Consume(o : ?[8]) : [1] <combines> {\n\
             return o.valid\n\
         }\n\
         UsePair(p : Pair) : [8] <combines> {\n\
             return p.data\n\
         }\n\
         module M {\n\
             reg opt : ?[8] = false\n\
             reg q : Pair = Pair{ valid: 1, data: 8'd7 }\n\
             out ok : [1] = 0\n\
             out v : [8] = 0\n\
             rule r {\n\
                 ok := Consume(8'd5)\n\
                 v := UsePair(q)\n\
             }\n\
         }\n",
    );
}

#[test]
fn option_and_struct_fn_returns_type_check() {
    // A struct/`?T`-typed fn RETURN is now supported too (`compile_
    // callee_body_field`, calls.rs, decomposes it one leaf field at a
    // time -- an emission-side concern, not a type-checking one here).
    run_ok(
        "struct Pair {\n\
             valid : [1]\n\
             data : [8]\n\
         }\n\
         Wrap(x : [8]) : ?[8] <combines> {\n\
             return x\n\
         }\n\
         MakePair() : Pair <combines> {\n\
             return Pair{ valid: 1, data: 8'd7 }\n\
         }\n\
         module M {\n\
             reg opt : ?[8] = false\n\
             reg p : Pair = Pair{ valid: 0, data: 0 }\n\
             rule r {\n\
                 opt := Wrap(8'd5)\n\
                 p := MakePair()\n\
             }\n\
         }\n",
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

/// `if`'s `allow_bare_comparison` exemption (`check_cond`), and `while`'s
/// own identical one added later (Lumi's call: "while [should] work like
/// if") — both accept a bare comparison directly as their own condition.
#[test]
fn if_and_while_both_accept_a_bare_comparison_as_their_own_condition() {
    run_ok(
        "module M {\n reg v : [8] = 0\n in a : [8]\n in b : [8]\n \
         rule r {\n if a > b {\n v := 1\n } else {\n v := 2\n }\n }\n}\n",
    );
    // `while`'s own condition gets the identical `allow_bare_comparison`
    // exemption `if`'s already had (Lumi's call: "while [should] work
    // like if"). A bare `logic x <> 0` here is no longer legal either
    // (Lumi's later, stricter call — see `a_bare_logic_wrapped_if_
    // condition_comparison_is_now_rejected_with_a_hint`, tests/firrtl.rs,
    // for the `if` version of the same rejection).
    run_ok(
        "Sum(x : [8]) : [8] <sequences, fails> {\n while x <> 0 {\n x := x >> 1\n }\n \
         return x\n}\n",
    );
}

/// The same widening extended to a bare fifo `Deq[]`/failing call
/// directly as a condition -- both fallible by default, same reasoning
/// as the comparison case just above, and now accepted by BOTH `if` and
/// `while` (Lumi's call: "while [should] work like if").
#[test]
fn if_and_while_both_accept_a_bare_fifo_deq_or_failing_call_as_their_own_condition() {
    run_ok(
        "module M {\n fifo f : [8]\n reg v : [8] = 0\n \
         rule r {\n if f.Deq[] {\n v := 1\n } else {\n v := 0\n }\n }\n}\n",
    );
    run_ok(
        "Classify(x : [8]) : [8] <combines, fails> {\n (x <> 0)?\n return x\n }\n\
         module M {\n reg v : [8] = 0\n in a : [8]\n \
         rule r {\n if Classify(a) {\n v := 1\n } else {\n v := 2\n }\n }\n}\n",
    );
    run_ok(
        "module M {\n fifo f : [8]\n reg v : [8] = 0\n \
         rule r <sequences> {\n while f.Deq[] {\n v := 1\n }\n }\n}\n",
    );
}

/// Advisor-caught while reviewing the `if`-only exemption above: a
/// comparison's own TYPE is its left operand's type (`type_binop`), so
/// once that operand happens to be exactly 1 bit wide, a comparison
/// COMBINED with `&`/`|`/`^` types as an ordinary `[1]` — indistinguish-
/// able from a genuine boolean by width alone. Pre-existing this
/// session's `if`-condition work entirely (reachable at `a01683f`
/// already, unrelated to `allow_bare_comparison`): `if (a > b) & c`
/// silently compiled to `mux(and(a, c), ...)`, using `a`'s own
/// passthrough value instead of `gt(a, b)` as the mux selector. Fixed by
/// `expr_has_undischarged_comparison` rejecting ANY undischarged
/// comparison reachable inside a condition that ISN'T exactly the whole
/// condition itself (or explicitly `logic`-discharged) — checked here at
/// the type level so it can never reach a compile-time mux-select bug.
#[test]
fn a_comparison_nested_inside_a_larger_if_or_while_condition_is_rejected_not_silently_miscompiled()
{
    let (_, _, errors) = run(
        "module M {\n reg v : [8] = 0\n in a : [1]\n in b : [1]\n in c : [1]\n \
         rule r {\n if (a > b) & c {\n v := 1\n }\n }\n}\n",
    );
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("wrap it with `logic`"));

    // `logic`-discharging the comparison FIRST is the escape hatch, same
    // idiom `logic A & logic B` already documents.
    run_ok(
        "module M {\n reg v : [8] = 0\n in a : [1]\n in b : [1]\n in c : [1]\n \
         rule r {\n if (logic a > b) & c {\n v := 1\n }\n }\n}\n",
    );

    // `while` now shares `if`'s `allow_bare_comparison` exemption too
    // (Lumi's call: "while [should] work like if"), so `while x <> 0`
    // with a 1-bit `x` is legitimately accepted now -- it's the WHOLE
    // condition, the exact shape the exemption covers, not the nested-
    // inside-a-larger-expression gap this test is about. That gap is
    // still very much alive for `while` too, though: a comparison
    // nested inside `&`/`|`/`^` is unconditionally rejected by `expr_
    // has_undischarged_comparison`, regardless of `allow_bare_
    // comparison` -- confirmed by direct probe before writing this.
    run_ok(
        "Sum(x : [1]) : [1] <sequences, fails> {\n while x <> 0 {\n x := 0\n }\n \
         return x\n}\n",
    );
    let (_, _, errors) = run(
        "module M {\n reg v : [8] = 0\n in a : [1]\n in b : [1]\n in c : [1]\n \
         rule r <sequences> {\n while (a > b) & c {\n v := 1\n }\n }\n}\n",
    );
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("wrap it with `logic`"));
}

/// The identical nested-comparison gap, one call site over: a bare
/// rule-body guard STATEMENT (`(complex)?`), which folds through
/// `compile_guard_unwrap_cond` (writes.rs) exactly like an if-condition
/// does, and has the same "only checks whether its own operand IS
/// directly a comparison" blind spot. `((a > b) & c)?` used to compile
/// clean with `fires_r = and(a, c)`, silently dropping `a > b`'s own
/// guard — the SAME `expr_has_undischarged_comparison` check in
/// `check_cond` closes this too, since this bare-statement position also
/// routes through `check_cond` (`type_stmt`'s `Stmt::Expr` arm).
#[test]
fn a_comparison_nested_inside_a_bare_guard_statement_is_rejected_not_silently_dropped() {
    let (_, _, errors) = run(
        "module M {\n reg v : [8] = 0\n in a : [1]\n in b : [1]\n in c : [1]\n \
         rule r {\n ((a > b) & c)?\n v := 1\n }\n}\n",
    );
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("wrap it with `logic`"));

    run_ok(
        "module M {\n reg v : [8] = 0\n in a : [1]\n in b : [1]\n in c : [1]\n \
         rule r {\n ((logic a > b) & c)?\n v := 1\n }\n}\n",
    );
}

/// `if let x = opt? { ... }`'s own `x` binds to `opt`'s UNWRAPPED type
/// (`type_expr`'s existing `Expr::Guard` arm, unchanged) -- checked here
/// by writing `x` into a target the WRONG width would reject.
#[test]
fn if_let_binds_the_unwrapped_option_type() {
    run_ok(
        "module M {\n reg opt : ?[8] = false\n reg v : [8] = 0\n \
         rule r {\n if let x = opt? {\n v := x\n }\n }\n}\n",
    );
    let (_, _, errors) = run("module M {\n reg opt : ?[8] = false\n reg v : [4] = 0\n \
         rule r {\n if let x = opt? {\n v := x\n }\n }\n}\n");
    assert_eq!(errors.len(), 1);
}

/// `if let`'s right-hand side must be an Option's own `?`-unwrap (v0
/// restriction: not a fifo op, failing call, or comparison, even though
/// each of those is ALSO `Expr::Guard`-compatible or otherwise fallible
/// in other positions) -- the scope this session's `AskUserQuestion`
/// picked (binding sugar only, no general fallible-binding chain).
#[test]
fn if_let_rhs_must_be_an_option_unwrap_not_another_fallible_shape() {
    // Missing `?` entirely.
    let (_, _, errors) = run("module M {\n reg opt : ?[8] = false\n reg v : [8] = 0\n \
         rule r {\n if let x = opt {\n v := x\n }\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("Option's own unwrap"));

    // A bare comparison -- not itself `Expr::Guard`, so it hits the same
    // message. Unlike a bare fifo `Deq[]`/failing call (see `if_let_
    // accepts_a_bare_fifo_deq_as_its_rhs`/`if_let_accepts_a_bare_failing_
    // call_as_its_rhs` below, both ACHIEVED v0 shapes), a bare comparison
    // as `if let`'s rhs is still explicitly out of scope -- `check_cond`'s
    // `allow_bare_comparison` exemption is `if`-only, never threaded
    // through `if let`'s own arm.
    let (_, _, errors) = run("module M {\n reg v : [8] = 0\n in a : [8]\n in b : [8]\n \
         rule r {\n if let x = a > b {\n v := x\n }\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("Option's own unwrap"));

    // A bare comparison, explicitly `?`-guarded (`Expr::Guard`, but its
    // own inner isn't `Ty::Option`) -- exercises the "IS `Expr::Guard`
    // but isn't an Option" branch specifically, not just "isn't `Expr::
    // Guard` at all".
    let (_, _, errors) = run("module M {\n reg v : [8] = 0\n in a : [8]\n in b : [8]\n \
         rule r {\n if let x = (a > b)? {\n v := x\n }\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("Option's own unwrap"));
}

/// `if let x = fifo.Deq[] { ... }` -- bare, no `?` (`Deq[]` is fallible
/// by default, same as a comparison, unlike an Option which needs an
/// explicit unwrap): type-checks clean, `x` bound to the fifo's own
/// element type. `Enq` is deliberately excluded -- there's no value to
/// bind a name to -- and still hits the ordinary "missing `?`" rejection
/// like any other non-Option, non-Deq shape.
#[test]
fn if_let_accepts_a_bare_fifo_deq_as_its_rhs() {
    let (_, _, errors) = run("module M {\n fifo f : [8]\n reg v : [8] = 0\n \
         rule r {\n if let x = f.Deq[] {\n v := x\n } else {\n v := 0\n }\n }\n}\n");
    assert_eq!(errors.len(), 0, "{errors:?}");

    let (_, _, errors) = run("module M {\n fifo f : [8]\n \
         rule r {\n if let x = f.Enq[5] {\n }\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("Option's own unwrap"));
}

/// `if let x = Classify(a) { ... }` -- bare, no `?` (a failing call is
/// fallible by default, same as `Deq[]`/a comparison): type-checks
/// clean, `x` bound to the callee's own return type. Needed threading
/// `fx` (computed effect signatures) into `TypeChecker`, which
/// previously had no way to know a call's `fails` status at all -- only
/// `resolve.rs`'s def kinds, which don't carry it.
#[test]
fn if_let_accepts_a_bare_failing_call_as_its_rhs() {
    let (_, _, errors) = run(
        "Classify(x : [8]) : [8] <combines, fails> {\n (x <> 0)?\n return x\n }\n\
         module M {\n reg v : [8] = 0\n in a : [8]\n \
         rule r {\n if let x = Classify(a) {\n v := x\n } else {\n v := 0\n }\n }\n}\n",
    );
    assert_eq!(errors.len(), 0, "{errors:?}");

    // A call to a NON-failing fn is still rejected -- there's nothing to
    // unwrap, and `is_failing_call` correctly says so.
    let (_, _, errors) = run("Identity(x : [8]) : [8] <combines> {\n return x\n }\n\
         module M {\n reg v : [8] = 0\n in a : [8]\n \
         rule r {\n if let x = Identity(a) {\n v := x\n }\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("Option's own unwrap"));
}

/// A struct-typed `if let` binding used as a WHOLE value (not `.field`
/// chase-through, which is out of scope entirely -- see the sibling
/// firrtl.rs test for that) already hits a DIFFERENT, pre-existing
/// restriction: a struct-typed write's right-hand side must be a fresh
/// literal, not a copied value (`type_write`'s existing check) --
/// confirms this is an inherited restriction `if let` gets for free, not
/// a gap it introduces.
#[test]
fn if_let_bound_struct_used_as_a_whole_value_hits_the_ordinary_struct_copy_restriction() {
    let src = "\
struct Pair {
    x : [8]
    y : [8]
}
module M {
    reg opt : ?Pair = false
    reg p : Pair = Pair{ x: 0, y: 0 }
    rule r {
        if let v = opt? {
            p := v
        } else {
            p := Pair{ x: 0, y: 0 }
        }
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("must be a struct literal"));
}

/// `while let v = opt? { ... }`'s own `v` binds to `opt`'s UNWRAPPED type,
/// same as `if let`'s equivalent test above -- `Stmt::WhileLet`'s arm
/// shares `type_expr`'s existing `Expr::Guard` handling.
#[test]
fn while_let_binds_the_unwrapped_option_type() {
    run_ok(
        "module M {\n reg opt : ?[8] = false\n reg v : [8] = 0\n \
         rule r {\n while let x = opt? {\n v := x\n }\n }\n}\n",
    );
    let (_, _, errors) = run("module M {\n reg opt : ?[8] = false\n reg v : [4] = 0\n \
         rule r {\n while let x = opt? {\n v := x\n }\n }\n}\n");
    assert_eq!(errors.len(), 1);
}

/// `while let`'s right-hand side must be an Option's own `?`-unwrap, same
/// v0 restriction as `if let`'s equivalent test above -- pins the
/// "while let"-worded error message `Stmt::WhileLet`'s arm produces.
#[test]
fn while_let_rhs_must_be_an_option_unwrap_not_another_fallible_shape() {
    // Missing `?` entirely.
    let (_, _, errors) = run("module M {\n reg opt : ?[8] = false\n reg v : [8] = 0\n \
         rule r {\n while let x = opt {\n v := x\n }\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("Option's own unwrap"));

    // A fifo op -- not itself `Expr::Guard`, so it hits the same message.
    let (_, _, errors) = run("module M {\n fifo f : [8]\n reg v : [8] = 0\n \
         rule r {\n while let x = f.Deq[] {\n v := x\n }\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("Option's own unwrap"));

    // A bare comparison, explicitly `?`-guarded (`Expr::Guard`, but its
    // own inner isn't `Ty::Option`).
    let (_, _, errors) = run("module M {\n reg v : [8] = 0\n in a : [8]\n in b : [8]\n \
         rule r {\n while let x = (a > b)? {\n v := x\n }\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("Option's own unwrap"));
}

#[test]
fn where_bound_init_satisfying_the_bound_is_accepted() {
    let (_, _, errors) = run("module M {\n reg i : [4] where i < 10 = 0\n}\n");
    assert!(errors.is_empty(), "{errors:?}");
}

// Stage 3's own operator generalization (v21): `check_where_bound_init`
// used to assume `bound`'s own shape was always `Binary { Lt, self, K
// }` (the parser's own v0-through-v20 restriction) -- it now shares
// `ast::normalize_where_relation` with `bounds.rs`, so the init-value
// check must read `>=`/`>`/commuted forms with the SAME numeric
// interpretation `bounds.rs` later checks writes against. Found and
// fixed empirically, not just designed: the first version of this
// generalization left `check_where_bound_init` untouched, and it
// silently mis-evaluated `where cnt >= 5`'s own init check against the
// wrong (pre-generalization) interval before this was caught.
#[test]
fn where_bound_ge_init_satisfying_the_bound_is_accepted() {
    let (_, _, errors) = run("module M {\n reg i : [4] where i >= 5 = 5\n}\n");
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn where_bound_ge_init_violating_the_bound_is_an_error() {
    let (_, _, errors) = run("module M {\n reg i : [4] where i >= 5 = 3\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("does not satisfy the declared bound")
    );
}

#[test]
fn commuted_where_bound_init_is_checked_with_the_correct_interval() {
    // `10 > cnt` means `cnt < 10`, self on the RHS -- the init check
    // must recognize this, not just the LHS-self case.
    let (_, _, errors) = run("module M {\n reg i : [4] where 10 > i = 12\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("does not satisfy the declared bound")
    );
}

#[test]
fn where_bound_init_violating_the_bound_is_an_error() {
    let (_, _, errors) = run("module M {\n reg i : [4] where i < 10 = 12\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("does not satisfy the declared bound")
    );
}

#[test]
fn where_bound_init_exactly_at_the_limit_is_an_error() {
    // `< 10` excludes 10 itself -- an off-by-one regression here would
    // silently accept the boundary value.
    let (_, _, errors) = run("module M {\n reg i : [4] where i < 10 = 10\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("does not satisfy the declared bound")
    );
}

#[test]
fn where_bound_two_sided_init_inside_the_range_is_accepted() {
    let (_, _, errors) = run("module M {\n reg j : [4] where 5 <= j < 10 = 5\n}\n");
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn where_bound_two_sided_init_below_the_lower_end_is_an_error() {
    let (_, _, errors) = run("module M {\n reg j : [4] where 5 <= j < 10 = 4\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("does not satisfy the declared bound")
    );
}

#[test]
fn where_bound_lower_end_must_be_a_compile_time_constant() {
    // `n` resolves fine (an ordinary reg), but `const_eval` never
    // consults runtime state -- only elaboration-time env bindings --
    // so a bare reg reference in the lower-end position must be its
    // own clean error, not silently treated as some fallback value.
    let (_, _, errors) =
        run("module M {\n reg n : [4] = 3\n reg j : [4] where n <= j < 10 = 5\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("lower end must be a compile-time constant")
    );
}

#[test]
fn where_bound_degenerate_range_is_rejected() {
    // `lower >= limit` is an empty range -- no init could ever satisfy
    // it, so this must be its own clean error, not silently accepted or
    // conflated with the ordinary "init doesn't satisfy" message.
    let (_, _, errors) = run("module M {\n reg j : [4] where 10 <= j < 10 = 5\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("strictly less than its upper end")
    );
}

#[test]
fn where_bound_on_an_output_init_satisfying_the_bound_is_accepted() {
    // v8: `where` extended to `out` -- the same init base-case check
    // `reg` already gets.
    let (_, _, errors) = run("module M {\n out i : [4] where i < 10 = 0\n}\n");
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn where_bound_on_an_output_init_violating_the_bound_is_an_error() {
    let (_, _, errors) = run("module M {\n out i : [4] where i < 10 = 12\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("does not satisfy the declared bound")
    );
}

#[test]
fn implicit_trunc_argument_width_solves_from_the_write_targets_own_type() {
    // v24: a 1-arg `trunc(value)` passed as an argument to a generic fn
    // has no width of its own (`Ty::Bits(Width::Unknown)`) -- but
    // `Stmt::Assign`'s own declared LHS type is threaded into `type_
    // call` as a `hint`, letting `Double`'s own `n` solve from matching
    // `[n + 1]` (the return shape) against `b`'s own `[5]` (purely
    // type-level, no value-range reasoning at all): `n = 4`. Confirms
    // both that `type_call` accepts the call AND that the backward-fill
    // actually wrote a concrete width into `trunc(b)`'s own `expr_tys`
    // entry (not just used it locally to check assignability), since
    // that's what FIRRTL emission's own separate resolver needs.
    let src = "\
module M {
    in a : [1]
    out b : [5] = 0
    Double(x : [n]) : [n + 1] { return x << 1 }
    rule step {
        a?
        b := Double(trunc(b))
    }
}
";
    let (ast, types, errors) = run(src);
    assert!(errors.is_empty(), "type errors: {errors:?}");
    let trunc_call = ast
        .exprs
        .iter()
        .enumerate()
        .find_map(|(i, e)| match e {
            trace::ast::Expr::Call { args, .. } if args.len() == 1 => {
                Some(trace::ast::ExprId(i as u32))
            }
            _ => None,
        })
        .expect("expected exactly one 1-arg call in this program");
    assert_eq!(
        types.expr_tys.get(&trunc_call),
        Some(&trace::types::Ty::Bits(trace::types::Width::Known(4)))
    );
}

#[test]
fn implicit_trunc_width_hint_declines_on_a_non_invertible_return_shape() {
    // Negative control on `invert_implicit_width`'s own deliberately
    // narrow scope (bare param, or param +/- a constant only): `Double`
    // here returns `[n * 2]`, which the inverter refuses to solve (no
    // `Mul` arm) rather than guess. This must NOT surface as a type
    // error on its own -- FIRRTL emission's separate, pre-existing
    // top-down hint mechanism (predating this feature, `firrtl/calls.
    // rs`) still resolves `n` independently at emission time, so
    // types.rs correctly has nothing to say here either way; this test
    // pins that the new hint mechanism's restraint doesn't regress that
    // pre-existing path into a spurious error.
    let src = "\
module M {
    in a : [1]
    out b : [8] = 0
    Double(x : [n]) : [n * 2] { return x }
    rule step {
        a?
        b := Double(trunc(b))
    }
}
";
    let (_, _, errors) = run(src);
    assert!(errors.is_empty(), "type errors: {errors:?}");
}
