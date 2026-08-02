use trace::ast::{Ast, Item};
use trace::{lexer, parser};

fn parse_ok(src: &str) -> Ast {
    let (tokens, lex_errors) = lexer::lex(src);
    assert!(lex_errors.is_empty(), "lex errors: {lex_errors:?}");
    let (ast, errors) = parser::parse(src, &tokens);
    assert!(errors.is_empty(), "parse errors: {errors:?}");
    ast
}

/// Parse `src` as the body of a one-statement rule; return the s-expr of
/// that statement's expression (or assignment).
fn stmt_sexpr(stmt_src: &str) -> String {
    let src = format!("rule t {{\n{stmt_src}\n}}\n");
    let ast = parse_ok(&src);
    let Item::Rule { body, .. } = ast.item(ast.roots[0]) else {
        panic!("expected rule");
    };
    assert_eq!(body.len(), 1, "expected one statement");
    match ast.stmt(body[0]) {
        trace::ast::Stmt::Expr(e) => ast.expr_sexpr(*e),
        trace::ast::Stmt::Assign { lhs, rhs } => {
            format!("(:= {} {})", ast.expr_sexpr(*lhs), ast.expr_sexpr(*rhs))
        }
        other => panic!("unexpected statement: {other:?}"),
    }
}

#[test]
fn fifo_bridge_example_parses() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/fifo_bridge.tr"
    ))
    .unwrap();
    let ast = parse_ok(&src);
    assert_eq!(
        ast.dump(),
        "\
module FifoBridge
  fifo input : (index bits 8)
  fifo output : (index bits 8)
  rule transfer
    (let x (index (. input Deq)))
    (index (. output Enq) x)
",
    );
}

#[test]
fn precedence_matches_rust_not_c() {
    assert_eq!(stmt_sexpr("x := a + b * c"), "(:= x (+ a (* b c)))");
    assert_eq!(stmt_sexpr("x := a & b = c"), "(:= x (= (& a b) c))");
    assert_eq!(stmt_sexpr("x := a >> 1 + b"), "(:= x (>> a (+ 1 b)))");
}

#[test]
fn arith_shift_parses_at_the_same_precedence_as_shl_shr() {
    // `>>>` is lexed greedily before `>>` (logos's own longest-match
    // rule, not a manual priority) and shares `<<`/`>>`'s binding power.
    assert_eq!(stmt_sexpr("x := a >>> 3"), "(:= x (>>> a 3))");
    assert_eq!(stmt_sexpr("x := a >>> 1 + b"), "(:= x (>>> a (+ 1 b)))");
    assert_eq!(stmt_sexpr("x := a >> 1 >>> 2"), "(:= x (>>> (>> a 1) 2))");
}

#[test]
fn range_binds_loosest() {
    assert_eq!(stmt_sexpr("x := 0..N-1"), "(:= x (.. 0 (- N 1)))");
}

#[test]
fn indexed_part_select_binds_as_loosely_as_range() {
    // `+:`/`-:` are semantic siblings of `..` (each only meaningful as a
    // `Bracket`'s own argument) and share its binding power, so a
    // complex `base`/`width` expression on either side parses whole
    // before the part-select operator applies, same as `0..N-1` above.
    assert_eq!(
        stmt_sexpr("x := a[base + 1 +: width - 1]"),
        "(:= x (index a (+: (+ base 1) (- width 1))))"
    );
    assert_eq!(
        stmt_sexpr("x := a[base + 1 -: width - 1]"),
        "(:= x (index a (-: (+ base 1) (- width 1))))"
    );
}

#[test]
fn sized_integer_literals_parse() {
    assert_eq!(stmt_sexpr("x := 8'd6"), "(:= x 8'd6)");
    assert_eq!(stmt_sexpr("x := 8'hFF"), "(:= x 8'd255)");
    assert_eq!(stmt_sexpr("x := 8'b1010"), "(:= x 8'd10)");
    assert_eq!(stmt_sexpr("x := 8'o17"), "(:= x 8'd15)");
    assert_eq!(stmt_sexpr("x := 8'6"), "(:= x 8'd6)");
    assert_eq!(stmt_sexpr("x := 16'd1_000"), "(:= x 16'd1000)");
}

#[test]
fn guard_postfix() {
    assert_eq!(stmt_sexpr("(mode = Draining)?"), "(? (= mode Draining))");
    assert_eq!(stmt_sexpr("reqs[i]?"), "(? (index reqs i))");
}

#[test]
fn calls_and_fields_chain() {
    assert_eq!(
        stmt_sexpr("x := input.Deq[]"),
        "(:= x (index (. input Deq)))"
    );
    assert_eq!(
        stmt_sexpr("x := pack(h1.result, h2.result)"),
        "(:= x (call pack (. h1 result) (. h2 result)))"
    );
    assert_eq!(stmt_sexpr("x := m[pc + 1]"), "(:= x (index m (+ pc 1)))");
}

#[test]
fn decl_keywords_are_contextual() {
    // `mem` is a keyword only at declaration position; DESIGN.md's Rmw
    // example reads and writes a state named `mem`.
    assert_eq!(
        stmt_sexpr("mem[addr] := v + 1"),
        "(:= (index mem addr) (+ v 1))"
    );
    let ast = parse_ok("rule r <reads {pc, mem}> {\n tick\n}\n");
    let Item::Rule { effects, .. } = ast.item(ast.roots[0]) else {
        panic!()
    };
    assert_eq!(effects[0].args, ["pc", "mem"]);
}

#[test]
fn spawn_prefix() {
    assert_eq!(
        stmt_sexpr("h1 := spawn ReadBank(bank0, pc)"),
        "(:= h1 (spawn (call ReadBank bank0 pc)))"
    );
}

#[test]
fn bare_tick_is_still_one_statement() {
    let src = "rule t {\n tick\n}\n";
    let ast = parse_ok(src);
    let Item::Rule { body, .. } = ast.item(ast.roots[0]) else {
        panic!("expected rule");
    };
    assert_eq!(body.len(), 1);
    assert!(matches!(ast.stmt(body[0]), trace::ast::Stmt::Tick));
}

#[test]
fn tick_with_trailing_expr_desugars_to_two_statements() {
    // `tick <expr>` is sugar for a bare `tick` immediately followed by
    // `<expr>` as its own statement — not a new AST shape lower.rs would
    // need to know about.
    let src = "rule t {\n tick sync[h1, h2]\n}\n";
    let ast = parse_ok(src);
    let Item::Rule { body, .. } = ast.item(ast.roots[0]) else {
        panic!("expected rule");
    };
    assert_eq!(body.len(), 2, "tick + the trailing expr as its own stmt");
    assert!(matches!(ast.stmt(body[0]), trace::ast::Stmt::Tick));
    match ast.stmt(body[1]) {
        trace::ast::Stmt::Expr(e) => {
            assert_eq!(ast.expr_sexpr(*e), "(index sync h1 h2)");
        }
        other => panic!("unexpected statement: {other:?}"),
    }
}

#[test]
fn tick_with_guard_expr_parses_same_as_a_separate_guard_statement() {
    let fused = "rule t {\n tick (x = 0)?\n}\n";
    let split = "rule t {\n tick\n (x = 0)?\n}\n";
    let fused_ast = parse_ok(fused);
    let split_ast = parse_ok(split);
    let Item::Rule { body: fb, .. } = fused_ast.item(fused_ast.roots[0]) else {
        panic!()
    };
    let Item::Rule { body: sb, .. } = split_ast.item(split_ast.roots[0]) else {
        panic!()
    };
    assert_eq!(fb.len(), 2);
    assert_eq!(sb.len(), 2);
    for (a, b) in [(fb[0], sb[0]), (fb[1], sb[1])] {
        let sexpr = |ast: &Ast, s| match ast.stmt(s) {
            trace::ast::Stmt::Expr(e) => ast.expr_sexpr(*e),
            trace::ast::Stmt::Tick => "tick".to_string(),
            other => panic!("unexpected statement: {other:?}"),
        };
        assert_eq!(sexpr(&fused_ast, a), sexpr(&split_ast, b));
    }
}

#[test]
fn rule_with_effects() {
    let ast = parse_ok("rule step <sequences, reads {pc, mem}, writes {mem}> {\n tick\n}\n");
    let Item::Rule { effects, body, .. } = ast.item(ast.roots[0]) else {
        panic!("expected rule");
    };
    let names: Vec<_> = effects.iter().map(|e| e.name.text.as_str()).collect();
    assert_eq!(names, ["sequences", "reads", "writes"]);
    assert_eq!(effects[1].args, ["pc", "mem"]);
    assert_eq!(body.len(), 1);
}

#[test]
fn function_with_signature() {
    let ast = parse_ok("Parity(x : bits[8]) : bits[1] <combines> {\n return x[0] ^ x[1]\n}\n");
    let Item::Fn {
        name,
        kind,
        params,
        ret,
        effects,
        body,
    } = ast.item(ast.roots[0])
    else {
        panic!("expected fn");
    };
    assert_eq!(*kind, trace::ast::FnKind::Fn);
    assert_eq!(name.text, "Parity");
    assert_eq!(params.len(), 1);
    assert_eq!(ast.expr_sexpr(params[0].ty), "(index bits 8)");
    assert_eq!(ast.expr_sexpr(ret.unwrap()), "(index bits 1)");
    assert_eq!(effects[0].name, "combines");
    assert_eq!(body.len(), 1);
}

#[test]
fn spec_and_impl_refines() {
    // Multiline impl signature straight from DESIGN.md: newlines before
    // `refines` and before the body brace.
    let src = "\
spec AnyGrant(reqs : bits[N]) : bits[clog2(N)] <combines, chooses> {
    return 0
}

impl RoundRobin(reqs : bits[N]) : bits[clog2(N)] <combines>
    refines AnyGrant
{
    return 0
}
";
    let ast = parse_ok(src);
    let Item::Fn { kind, .. } = ast.item(ast.roots[0]) else {
        panic!()
    };
    assert_eq!(*kind, trace::ast::FnKind::Spec);
    let Item::Fn { kind, effects, .. } = ast.item(ast.roots[1]) else {
        panic!()
    };
    let trace::ast::FnKind::Impl { refines } = kind else {
        panic!("expected impl, got {kind:?}");
    };
    assert_eq!(refines.text, "AnyGrant");
    assert_eq!(effects[0].name, "combines");
}

#[test]
fn schedule_block() {
    let src = "\
schedule {
    urgency step > refill > idle
    mutually_exclusive { set_a, set_b }
    conflict_free { read_port, write_port }
}
";
    let ast = parse_ok(src);
    let Item::Schedule { directives } = ast.item(ast.roots[0]) else {
        panic!()
    };
    let flat: Vec<(&str, Vec<&str>)> = directives
        .iter()
        .map(|d| match d {
            trace::ast::ScheduleDirective::Urgency(ns) => {
                ("urgency", ns.iter().map(|n| n.text.as_str()).collect())
            }
            trace::ast::ScheduleDirective::MutuallyExclusive(ns) => (
                "mutually_exclusive",
                ns.iter().map(|n| n.text.as_str()).collect(),
            ),
            trace::ast::ScheduleDirective::ConflictFree(ns) => (
                "conflict_free",
                ns.iter().map(|n| n.text.as_str()).collect(),
            ),
        })
        .collect();
    assert_eq!(
        flat,
        [
            ("urgency", vec!["step", "refill", "idle"]),
            ("mutually_exclusive", vec!["set_a", "set_b"]),
            ("conflict_free", vec!["read_port", "write_port"]),
        ]
    );
}

#[test]
fn schedule_rejects_unknown_directive() {
    let src = "schedule {\n priority a > b\n}\n";
    let (tokens, _) = lexer::lex(src);
    let (_, errors) = parser::parse(src, &tokens);
    assert!(errors.iter().any(|e| e.message.contains("priority")));
}

#[test]
fn all_examples_parse() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples");
    let mut found = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "tr") {
            let src = std::fs::read_to_string(&path).unwrap();
            let (tokens, lex_errors) = lexer::lex(&src);
            assert!(
                lex_errors.is_empty(),
                "lex errors in {path:?}: {lex_errors:?}"
            );
            let (_, errors) = parser::parse(&src, &tokens);
            assert!(errors.is_empty(), "parse errors in {path:?}: {errors:?}");
            found += 1;
        }
    }
    assert!(
        found >= 3,
        "expected at least 3 example files, found {found}"
    );
}

#[test]
fn if_else_chain() {
    let ast = parse_ok("rule t {\n if r <= 0 { pc := c } else { pc := pc + 3 }\n}\n");
    assert_eq!(
        ast.dump(),
        "\
rule t
  if (<= r 0)
    (:= pc c)
  else
    (:= pc (+ pc 3))
",
    );
}

#[test]
fn reg_with_init_and_mem() {
    let ast = parse_ok("module M {\n reg pc : bits[16] = 0\n mem m : bits[16][4096]\n}\n");
    assert_eq!(
        ast.dump(),
        "\
module M
  reg pc : (index bits 16) = 0
  mem m : (index (index bits 16) 4096)
",
    );
    // reg init survives
    let Item::Module { items, .. } = ast.item(ast.roots[0]) else {
        panic!()
    };
    let Item::Reg { init, .. } = ast.item(items[0]) else {
        panic!()
    };
    assert!(init.is_some());
}

#[test]
fn reg_and_output_infer_type_from_a_sized_literal_init() {
    // Omitting `: ty` when initialized with a sized literal synthesizes
    // the identical `bits[width]` an explicit annotation would parse to.
    let ast = parse_ok("module M {\n reg a = 8'd6\n out b = 16'hFF00\n}\n");
    assert_eq!(
        ast.dump(),
        "\
module M
  reg a : (index bits 8) = 8'd6
  out b : (index bits 16) = 16'd65280
",
    );
}

#[test]
fn reg_without_type_or_sized_literal_init_is_an_error() {
    for src in ["module M {\n reg a\n}\n", "module M {\n reg a = 6\n}\n"] {
        let (tokens, _) = lexer::lex(src);
        let (_, errors) = parser::parse(src, &tokens);
        assert!(!errors.is_empty(), "expected an error for {src:?}");
    }
}

#[test]
fn mem_fifo_in_inst_still_require_an_explicit_type() {
    // Type inference is reg/out-only (the only decls with an `= init`);
    // every other decl kind must still spell `: ty` out.
    for src in [
        "module M {\n mem m 8\n}\n",
        "module M {\n fifo f 8\n}\n",
        "module M {\n in i 8\n}\n",
        "module M {\n inst i Child\n}\n",
    ] {
        let (tokens, _) = lexer::lex(src);
        let (_, errors) = parser::parse(src, &tokens);
        assert!(!errors.is_empty(), "expected an error for {src:?}");
        assert!(
            errors[0].message.contains("before type"),
            "{src:?}: {errors:?}"
        );
    }
}

#[test]
fn input_output_ports() {
    let ast = parse_ok(
        "module M {\n in inc : bits[8]\n out sum : bits[8] = 0\n\
         rule r {\n sum := sum + inc\n}\n}\n",
    );
    assert_eq!(
        ast.dump(),
        "\
module M
  in inc : (index bits 8)
  out sum : (index bits 8) = 0
  rule r
    (:= sum (+ sum inc))
",
    );
}

#[test]
fn multi_tick_rule() {
    let ast = parse_ok("rule step <sequences> {\n a := m[pc]\n tick\n b := m[pc + 1]\n}\n");
    let Item::Rule { body, .. } = ast.item(ast.roots[0]) else {
        panic!()
    };
    assert_eq!(body.len(), 3);
    assert!(matches!(ast.stmt(body[1]), trace::ast::Stmt::Tick));
}

#[test]
fn error_recovery_continues_past_bad_statement() {
    let src = "rule t {\n x := := y\n z := 1\n}\n";
    let (tokens, _) = lexer::lex(src);
    let (ast, errors) = parser::parse(src, &tokens);
    assert!(!errors.is_empty());
    // The good statement after the bad one still parses.
    let Item::Rule { body, .. } = ast.item(ast.roots[0]) else {
        panic!()
    };
    assert!(
        body.iter()
            .any(|s| matches!(ast.stmt(*s), trace::ast::Stmt::Assign { .. }))
    );
}

#[test]
fn stray_tokens_terminate() {
    // Regression: a stray `}` at top level (or any unconsumed token after
    // recovery) must not loop forever. Exercises both recovery loops.
    for src in ["}\n", "}}}}\n", "rule t {\n x := (1 <\n}\n extra }"] {
        let (tokens, _) = lexer::lex(src);
        let (_, errors) = parser::parse(src, &tokens);
        assert!(!errors.is_empty(), "expected errors for {src:?}");
        assert!(errors.len() < 20, "runaway errors for {src:?}");
    }
}

#[test]
fn one_error_not_a_cascade() {
    let src = "rule t {\n x := (a +\n}\n";
    let (tokens, _) = lexer::lex(src);
    let (_, errors) = parser::parse(src, &tokens);
    assert!(!errors.is_empty());
    assert!(errors.len() <= 2, "error cascade: {errors:?}");
}

#[test]
fn bit_is_pure_sugar_for_bits_1() {
    // `bit` desugars in the parser to the IDENTICAL AST a literal
    // `bits[1]` produces (`Bracket { Ident("bits"), [Int(1)] }`) — proved
    // here at the AST level so every downstream pass (resolve/effects/
    // types/emission) can stay unaware `bit` was ever written, the same
    // "needed no changes anywhere but parser.rs" property the sized-
    // literal-inferred-`reg`-type feature already established.
    assert_eq!(stmt_sexpr("x := bit"), stmt_sexpr("x := bits[1]"));
    // Bracket-applied, `bit` behaves as a mem element type exactly like
    // `bits[1]` would (`mem m : bit[16]` == `mem m : bits[1][16]`).
    assert_eq!(stmt_sexpr("x := bit[16]"), stmt_sexpr("x := bits[1][16]"));
}

#[test]
fn u_n_is_sugar_for_bits_n() {
    // `u8`/`u16`/... desugar to the identical `Bracket { Ident("bits"),
    // [N] }` shape a literal `bits[N]` would produce -- same treatment
    // as `bit`, but matched on an already-lexed `Ident`'s own text
    // (`u_width`, parser.rs) rather than a fixed keyword, since `N` is
    // unbounded.
    assert_eq!(stmt_sexpr("x := u8"), stmt_sexpr("x := bits[8]"));
    assert_eq!(stmt_sexpr("x := u32"), stmt_sexpr("x := bits[32]"));
    // Not sugar: no digits, or a non-digit suffix -- an ordinary ident.
    assert_eq!(stmt_sexpr("x := u"), "(:= x u)");
    assert_eq!(stmt_sexpr("x := unused"), "(:= x unused)");
    assert_eq!(stmt_sexpr("x := u8x"), "(:= x u8x)");
}

#[test]
fn in_out_are_contextual_like_mem() {
    // `in`/`out` are keywords only at declaration position, the same
    // fallback `mem`/`fifo`/`reg` already get (see
    // `decl_keywords_are_contextual`) -- a fifo literally named `in`/
    // `out` (DESIGN.md's own `FifoBridge`-style naming, just shorter)
    // still resolves as a plain identifier everywhere else.
    assert_eq!(stmt_sexpr("out.Enq[x]"), "(index (. out Enq) x)");
    assert_eq!(stmt_sexpr("x := in.Deq[]"), "(:= x (index (. in Deq)))");
}

#[test]
fn list_literal_parses_as_a_list_node() {
    assert_eq!(stmt_sexpr("x := [a, b, c]"), "(:= x (list a b c))");
}

#[test]
fn one_sided_ranges_parse_with_the_omitted_side_absent() {
    // `xs[..mid]` and `xs[mid..]` -- the one-sided range form exclusive
    // to list slicing (DESIGN.md's `AdderTree` example), distinct from
    // the pre-existing two-sided `BinOp::Range` bit-slice.
    assert_eq!(stmt_sexpr("x := xs[..mid]"), "(:= x (index xs (..  mid)))");
    assert_eq!(stmt_sexpr("x := xs[mid..]"), "(:= x (index xs (.. mid )))");
}

#[test]
fn a_two_sided_range_still_parses_as_the_ordinary_bit_slice_binop() {
    // Guards against the one-sided `..mid`/`mid..` parser additions
    // accidentally shadowing the pre-existing two-sided `x[hi..lo]` bit-
    // slice, which stays a plain `Binary { Range, .. }`, not the new
    // one-sided `Range { lo: Option, hi: Option }` node.
    assert_eq!(stmt_sexpr("x := y[7..0]"), "(:= x (index y (.. 7 0)))");
}

#[test]
fn a_trailing_range_at_end_of_input_does_not_consume_past_the_statement() {
    // The infix `mid..` lookahead must not eat tokens that begin the
    // NEXT statement -- proved by a second statement following it.
    let src = "rule t {\n x := xs[mid..]\n y := 1\n}\n";
    let ast = parse_ok(src);
    let trace::ast::Item::Rule { body, .. } = ast.item(ast.roots[0]) else {
        panic!("expected rule");
    };
    assert_eq!(body.len(), 2, "expected two statements: {body:?}");
}

#[test]
fn if_with_a_bare_ident_condition_still_parses_its_body_as_a_block_not_a_struct_literal() {
    // `at_struct_lit_open` disambiguates `Name{ field: value }` (a
    // struct literal) from `if cond { x := 1 }` (a block body) by
    // requiring `ident :` -- not `ident :=` -- right after the `{`. A
    // bare-ident condition followed by an assignment statement is
    // exactly the case that could collide: `cond { x := 1 }` has `{`
    // immediately followed by an ident, same as a struct literal's
    // opening shape.
    let src = "rule t {\n if cond {\n x := 1\n }\n}\n";
    let ast = parse_ok(src);
    let trace::ast::Item::Rule { body, .. } = ast.item(ast.roots[0]) else {
        panic!("expected rule");
    };
    assert_eq!(body.len(), 1, "expected one statement: {body:?}");
    let trace::ast::Stmt::If { then_body, .. } = ast.stmt(body[0]) else {
        panic!("expected an if statement, got: {:?}", ast.stmt(body[0]));
    };
    assert_eq!(
        then_body.len(),
        1,
        "expected one statement in the then-body"
    );
    assert!(matches!(
        ast.stmt(then_body[0]),
        trace::ast::Stmt::Assign { .. }
    ));
}

#[test]
fn while_with_a_bare_ident_condition_still_parses_its_body_as_a_block_not_a_struct_literal() {
    // `while`'s condition is parsed by the exact same `parse_expr(0)`
    // call `if`'s condition is (see `parse_stmt`), so it shares the
    // same struct-literal-vs-block-body collision risk -- pinned
    // separately since the two aren't literally the same code path
    // (`parse_if` vs. the `While` arm), just the same expression parse.
    let src = "rule t {\n while cond {\n x := 1\n }\n}\n";
    let ast = parse_ok(src);
    let trace::ast::Item::Rule { body, .. } = ast.item(ast.roots[0]) else {
        panic!("expected rule");
    };
    assert_eq!(body.len(), 1, "expected one statement: {body:?}");
    let trace::ast::Stmt::While {
        body: while_body, ..
    } = ast.stmt(body[0])
    else {
        panic!("expected a while statement, got: {:?}", ast.stmt(body[0]));
    };
    assert_eq!(while_body.len(), 1, "expected one statement in the body");
    assert!(matches!(
        ast.stmt(while_body[0]),
        trace::ast::Stmt::Assign { .. }
    ));
}
