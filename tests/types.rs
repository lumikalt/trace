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
    run_ok("F(x : bits[8]) : bits[1] <converges> {\n return x[0] ^ x[7]\n}\n");
    run_ok("G(x : bits[16]) : bits[8] <converges> {\n return x[7..0]\n}\n");

    let (_, _, errors) = run("H(x : bits[16]) : bits[4] <converges> {\n return x[7..0]\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("bits[8]"));
}

#[test]
fn implicit_width_params_instantiate_at_call_sites() {
    // N solves to 8, so clog2(N) = 3: assigning to bits[3] works and
    // assigning to bits[2] fails.
    let src = "\
Enc(reqs : bits[N]) : bits[clog2(N)] <converges> {
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
Enc(reqs : bits[N]) : bits[clog2(N)] <converges> {
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
F(x : bits[8]) : bits[8] <converges> {
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
        "module M {\n reg a : bits[8] = 0\n reg b : bits[8] = 0\n rule r {\n if a != 0 { b := 1 }\n }\n}\n",
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
    run_ok("Mix(a : bits[N], b : bits[N]) : bits[N] <converges> {\n return (a & b) ^ (a | b)\n}\n");
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
