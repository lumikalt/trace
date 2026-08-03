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
fn io_port_parses_name_and_type() {
    let ast = parse_ok("module M {\n io bus : [8]\n}\n");
    let Item::Module { items, .. } = ast.item(ast.roots[0]) else {
        panic!()
    };
    let Item::Io { name, ty } = ast.item(items[0]) else {
        panic!("expected an io port");
    };
    assert_eq!(name.text, "bus");
    assert_eq!(ast.expr_sexpr(*ty), "(index bits 8)");
}

#[test]
fn io_port_requires_an_explicit_type() {
    let (tokens, _) = lexer::lex("module M {\n io bus\n}\n");
    let (_, errors) = parser::parse("module M {\n io bus\n}\n", &tokens);
    assert!(!errors.is_empty());
}

#[test]
fn io_port_rejects_an_initializer() {
    let (tokens, _) = lexer::lex("module M {\n io bus : [8] = 0\n}\n");
    let (_, errors) = parser::parse("module M {\n io bus : [8] = 0\n}\n", &tokens);
    assert!(!errors.is_empty());
}

#[test]
fn attach_parses_two_operands() {
    let ast = parse_ok("module M {\n io a : [8]\n io b : [8]\n attach a, b\n}\n");
    let Item::Module { items, .. } = ast.item(ast.roots[0]) else {
        panic!()
    };
    let Item::Attach { a, b } = ast.item(items[2]) else {
        panic!("expected an attach item");
    };
    assert_eq!(ast.expr_sexpr(*a), "a");
    assert_eq!(ast.expr_sexpr(*b), "b");
}

#[test]
fn attach_parses_an_instance_field_operand() {
    let ast = parse_ok("module M {\n io a : [8]\n inst c : Child\n attach a, c.bus\n}\n");
    let Item::Module { items, .. } = ast.item(ast.roots[0]) else {
        panic!()
    };
    let Item::Attach { b, .. } = ast.item(items[2]) else {
        panic!("expected an attach item");
    };
    assert_eq!(ast.expr_sexpr(*b), "(. c bus)");
}

#[test]
fn attach_requires_a_comma_between_operands() {
    let (tokens, _) = lexer::lex("module M {\n io a : [8]\n io b : [8]\n attach a b\n}\n");
    let (_, errors) = parser::parse(
        "module M {\n io a : [8]\n io b : [8]\n attach a b\n}\n",
        &tokens,
    );
    assert!(!errors.is_empty());
}

#[test]
fn extmodule_parses_name_path_and_ports() {
    let ast = parse_ok(
        "extmodule TriBuf from \"tribuf.v\" {\n in enable : [1]\n \
         out sensed : [8]\n io pad : [8]\n}\n",
    );
    let Item::ExtModule { name, path, ports } = ast.item(ast.roots[0]) else {
        panic!("expected an extmodule");
    };
    assert_eq!(name.text, "TriBuf");
    assert_eq!(path, "tribuf.v");
    assert_eq!(ports.len(), 3);
    assert_eq!(ports[0].dir, trace::ast::ExtPortDir::In);
    assert_eq!(ports[0].name.text, "enable");
    assert_eq!(ports[1].dir, trace::ast::ExtPortDir::Out);
    assert_eq!(ports[2].dir, trace::ast::ExtPortDir::Io);
}

#[test]
fn extmodule_requires_the_from_keyword() {
    let (tokens, _) = lexer::lex("extmodule TriBuf \"tribuf.v\" {\n}\n");
    let (_, errors) = parser::parse("extmodule TriBuf \"tribuf.v\" {\n}\n", &tokens);
    assert!(!errors.is_empty());
}

#[test]
fn extmodule_requires_a_quoted_path() {
    let (tokens, _) = lexer::lex("extmodule TriBuf from tribuf.v {\n}\n");
    let (_, errors) = parser::parse("extmodule TriBuf from tribuf.v {\n}\n", &tokens);
    assert!(!errors.is_empty());
}

#[test]
fn extmodule_rejects_a_port_with_no_direction_keyword() {
    let (tokens, _) = lexer::lex("extmodule TriBuf from \"tribuf.v\" {\n pad : [8]\n}\n");
    let (_, errors) = parser::parse(
        "extmodule TriBuf from \"tribuf.v\" {\n pad : [8]\n}\n",
        &tokens,
    );
    assert!(!errors.is_empty());
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

/// `rule foo?` — the optional/enable sugar (TODO.md's "Rules: optional/
/// enable sugar") — desugars at parse time into five items: the implicit
/// `in` port, its `__prev`-named shadow register (reset to `1`, not `0`,
/// so a port already held high at reset reads as no edge — Lumi's call,
/// via `AskUserQuestion`), an always-firing `__edge`-named rule updating
/// the shadow register, the original rule with a rising-edge guard
/// prepended, and a `conflict_free` exemption between the two rules (an
/// ordinary schedule.rs conflict otherwise, since both touch the shadow
/// register).
#[test]
fn optional_rule_sugar_desugars_to_five_items() {
    let ast = parse_ok("module M {\n rule step? {\n tick\n}\n}\n");
    assert_eq!(
        ast.dump(),
        "\
module M
  in step : (index bits 1)
  reg __prev_step : (index bits 1) = 1
  rule __edge_step
    (:= __prev_step step)
  rule step
    (? (& step (not __prev_step)))
    tick
  schedule
    (conflict_free __edge_step step)
",
    );
}

/// The `?` sits before any `<effects>` tag list (`rule foo? <reads {...}>
/// { ... }`), mirroring `?T`'s own use as a type marker — and the tag
/// list still ends up on the desugared rule itself, not lost or moved.
#[test]
fn optional_rule_sugar_keeps_its_effects_tag_list() {
    let ast = parse_ok("module M {\n rule step? <reads {pc}> {\n tick\n}\n}\n");
    let Item::Module { items, .. } = ast.item(ast.roots[0]) else {
        panic!("expected module");
    };
    assert_eq!(items.len(), 5);
    let Item::Rule { name, effects, .. } = ast.item(items[3]) else {
        panic!("expected the rewritten main rule as the 4th synthesized item");
    };
    assert_eq!(name.text, "step");
    assert_eq!(effects[0].name, "reads");
    assert_eq!(effects[0].args, ["pc"]);
}

/// `rule foo? <sequences>` is rejected at parse time, not left to panic
/// deep inside `lower.rs`. Every node the sugar synthesizes shares `foo`'s
/// own span (harmless for everything downstream that reads structure, not
/// source text) — except `lower.rs`, which reconstructs a `<sequences>`
/// rule's segments FROM spans, and hits a real "overlapping lowering
/// edits" panic on the collision (confirmed by hand before adding this
/// check). A v0 restriction: the manual `in foo : [1]` + `foo?` pattern
/// still works fine under `<sequences>` (see the `spawn`/`race` examples).
#[test]
fn optional_rule_sugar_rejects_sequences() {
    let src = "module M {\n reg count : [4] = 0\n rule step? <sequences> {\n \
               tick\n count := count + 1\n}\n}\n";
    let (tokens, lex_errors) = lexer::lex(src);
    assert!(lex_errors.is_empty(), "lex errors: {lex_errors:?}");
    let (_, errors) = parser::parse(src, &tokens);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("isn't supported yet"));
}

#[test]
fn function_with_signature() {
    let ast = parse_ok("Parity(x : [8]) : [1] <combines> {\n return x[0] ^ x[1]\n}\n");
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
spec AnyGrant(reqs : [N]) : [clog2(N)] <combines, chooses> {
    return 0
}

impl RoundRobin(reqs : [N]) : [clog2(N)] <combines>
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
    let ast = parse_ok("module M {\n reg pc : [16] = 0\n mem m : [16][4096]\n}\n");
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
    // the identical `[width]` an explicit annotation would parse to.
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
        "module M {\n in inc : [8]\n out sum : [8] = 0\n\
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
fn bit_and_u_n_are_now_ordinary_identifiers() {
    // `bit`/`uN` sugar for `[1]`/`[N]` was retired in favor of
    // the bare `[N]` type shorthand (see
    // `bracket_disambiguates_list_literal_from_bits_ty`, below) — both
    // are completely ordinary identifiers now, with zero parser-level
    // special-casing; the CLI's `--lower`/`--firrtl` paths reject them
    // downstream (`cannot find bit`/`cannot find u8`) exactly like any
    // other unbound name, not here at parse time.
    assert_eq!(stmt_sexpr("x := bit"), "(:= x bit)");
    assert_eq!(stmt_sexpr("x := u8"), "(:= x u8)");
    assert_eq!(stmt_sexpr("x := u32"), "(:= x u32)");
    assert_eq!(stmt_sexpr("x := u"), "(:= x u)");
    assert_eq!(stmt_sexpr("x := unused"), "(:= x unused)");
    assert_eq!(stmt_sexpr("x := u8x"), "(:= x u8x)");
}

#[test]
fn bracket_disambiguates_list_literal_from_bits_ty() {
    // A bare `[N]` — exactly one item, no trailing comma — is the
    // `bits[N]` type shorthand: the identical `Bracket { Ident("bits"),
    // [N] }` shape the now-retired literal `bits[N]` spelling used to
    // produce, so every downstream pass (resolve/effects/types/emission)
    // stays unaware the surface spelling ever changed.
    assert_eq!(stmt_sexpr("x := [1]"), "(:= x (index bits 1))");
    assert_eq!(stmt_sexpr("x := [8]"), "(:= x (index bits 8))");
    // Bracket-applied, `[1]` behaves as a mem element type exactly like
    // `bits[1]` used to (`mem m : [1][16]`).
    assert_eq!(
        stmt_sexpr("x := [1][16]"),
        "(:= x (index (index bits 1) 16))"
    );
    // An arbitrary width EXPRESSION, not just a literal int, still works
    // — the same generic-param/arithmetic width `[N]` always allowed.
    assert_eq!(stmt_sexpr("x := [N]"), "(:= x (index bits N))");
    assert_eq!(
        stmt_sexpr("x := [clog2(N)]"),
        "(:= x (index bits (call clog2 N)))"
    );
    // Empty, or 2+ comma-separated items, stays a list literal — told
    // apart by content, not position.
    assert_eq!(stmt_sexpr("x := []"), "(:= x (list))");
    assert_eq!(stmt_sexpr("x := [a, b]"), "(:= x (list a b))");
    assert_eq!(stmt_sexpr("x := [a, b, c]"), "(:= x (list a b c))");
    // A trailing comma is the escape hatch for a genuine one-element list
    // VALUE — the same role Rust's own one-element tuple syntax gives a
    // trailing comma, and for an identical reason: `[a]` alone is already
    // claimed (the `bits[N]` shorthand above).
    assert_eq!(stmt_sexpr("x := [a,]"), "(:= x (list a))");
}

#[test]
fn old_bits_n_spelling_is_a_clean_error() {
    let src = "rule t {\nx := bits[8]\n}\n";
    let (tokens, _) = lexer::lex(src);
    let (_, errors) = parser::parse(src, &tokens);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("no longer valid syntax"));
    assert!(errors[0].message.contains("use `[N]`"));
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
fn if_let_parses_name_init_and_both_branches() {
    // `if let NAME = EXPR { ... } else { ... }` -- Option-presence binding
    // sugar (DESIGN.md's "`if let`: branch-scoped Option-presence
    // binding"). `EXPR` parses as an ordinary expression (no shape
    // restriction here -- types.rs is what requires `Expr::Guard`).
    let src = "rule t {\n if let x = opt? {\n v := x\n } else {\n v := 0\n }\n}\n";
    let ast = parse_ok(src);
    let trace::ast::Item::Rule { body, .. } = ast.item(ast.roots[0]) else {
        panic!("expected rule");
    };
    assert_eq!(body.len(), 1, "expected one statement: {body:?}");
    let trace::ast::Stmt::IfLet {
        name,
        init,
        then_body,
        else_body,
    } = ast.stmt(body[0])
    else {
        panic!("expected an if-let statement, got: {:?}", ast.stmt(body[0]));
    };
    assert_eq!(name.text, "x");
    assert!(matches!(ast.expr(*init), trace::ast::Expr::Guard(_)));
    assert_eq!(then_body.len(), 1);
    assert_eq!(
        else_body.as_ref().map(|b| b.len()),
        Some(1),
        "expected an else body"
    );
}

#[test]
fn if_let_without_an_else_parses_with_none() {
    let src = "rule t {\n if let x = opt? {\n v := x\n }\n}\n";
    let ast = parse_ok(src);
    let trace::ast::Item::Rule { body, .. } = ast.item(ast.roots[0]) else {
        panic!("expected rule");
    };
    let trace::ast::Stmt::IfLet { else_body, .. } = ast.stmt(body[0]) else {
        panic!("expected an if-let statement, got: {:?}", ast.stmt(body[0]));
    };
    assert!(else_body.is_none());
}

#[test]
fn while_let_parses_name_init_and_body() {
    // `while let NAME = EXPR { ... }` -- the loop-shaped sibling of `if
    // let` (DESIGN.md's "`while let`: looping over Option presence").
    let src = "rule t {\n while let x = opt? {\n v := x\n }\n}\n";
    let ast = parse_ok(src);
    let trace::ast::Item::Rule { body, .. } = ast.item(ast.roots[0]) else {
        panic!("expected rule");
    };
    assert_eq!(body.len(), 1, "expected one statement: {body:?}");
    let trace::ast::Stmt::WhileLet { name, init, body } = ast.stmt(body[0]) else {
        panic!(
            "expected a while-let statement, got: {:?}",
            ast.stmt(body[0])
        );
    };
    assert_eq!(name.text, "x");
    assert!(matches!(ast.expr(*init), trace::ast::Expr::Guard(_)));
    assert_eq!(body.len(), 1);
}

#[test]
fn while_let_rejects_a_trailing_else() {
    // Unlike `if let`, `while let` has no `else`/`else if` handling at
    // all -- `parse_while_let` calls `expect_terminator()` right after
    // `parse_block()`, so a dangling `else` is a parse error, not a
    // silently-accepted no-op.
    let src = "rule t {\n while let x = opt? {\n v := x\n } else {\n v := 0\n }\n}\n";
    let (tokens, _) = lexer::lex(src);
    let (_, errors) = parser::parse(src, &tokens);
    assert!(
        !errors.is_empty(),
        "expected a parse error for trailing else on while let"
    );
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

#[test]
fn optional_is_a_real_prefix_node_distinct_from_its_wrapped_expression() {
    // Unlike `?T`'s type-position prefix `?`, `optional` is a real AST
    // node (not elided): it needs to survive into typing/emission so a
    // `??T`'s two `valid` bits can be driven independently (`Some(None)`).
    assert_eq!(stmt_sexpr("x := optional 5'd3"), "(:= x (optional 5'd3))");
    // Nests -- `optional (optional false)` is `Some(Some(absent))` on a
    // `???T`, one `optional` per layer of forced presence.
    assert_eq!(
        stmt_sexpr("x := optional optional false"),
        "(:= x (optional (optional false)))"
    );
}

#[test]
fn logic_is_a_prefix_operator_not_a_call() {
    // `logic <expr>` -- no parens needed, not through call syntax
    // (`logic` isn't in the `BUILTINS` identifier list at all anymore --
    // see resolve.rs).
    assert_eq!(
        stmt_sexpr("x := logic f.Deq[]"),
        "(:= x (logic (index (. f Deq))))"
    );
    // Old call-style parens still parse -- they're just grouping,
    // absorbed by the operand parse.
    assert_eq!(
        stmt_sexpr("x := logic(f.Deq[])"),
        "(:= x (logic (index (. f Deq))))"
    );
}

#[test]
fn logic_binds_looser_than_every_binary_operator() {
    // Unlike `not`/`optional` (`PREFIX_BP`, tighter than any binary
    // operator), `logic`'s operand parses at `0` -- deliberately loose,
    // so a bare comparison reads naturally: `logic a > b` is
    // `logic (a > b)`, not `(logic a) > b` (Lumi's call, trading away
    // the OTHER direction: see the next test).
    assert_eq!(stmt_sexpr("x := logic a > b"), "(:= x (logic (> a b)))");
    assert_eq!(stmt_sexpr("x := logic a + b"), "(:= x (logic (+ a b)))");
}

#[test]
fn logic_combined_with_amp_now_needs_parens_on_each_side() {
    // The trade Lumi picked when asking to lower `logic`'s precedence:
    // this language's bitwise operators bind TIGHTER than comparisons
    // (Rust-style, `precedence_matches_rust_not_c`), so no threshold
    // lets `logic` swallow a bare comparison without ALSO swallowing
    // `&`/`|`/`^` -- the `and`-combination idiom (`logic A & logic B`,
    // DESIGN.md) now parses as ONE `logic` wrapping the whole `&`
    // expression, not two separately-discharged fallible values.
    assert_eq!(
        stmt_sexpr("x := logic a & logic b"),
        "(:= x (logic (& a (logic b))))"
    );
    // Each side needs its own parens to keep the old two-`logic`
    // meaning.
    assert_eq!(
        stmt_sexpr("x := (logic a) & (logic b)"),
        "(:= x (& (logic a) (logic b)))"
    );
}

fn let_sexpr(ast: &Ast, s: trace::ast::StmtId) -> String {
    match ast.stmt(s) {
        trace::ast::Stmt::Let { name, init } => {
            format!("(let {} {})", name.text, ast.expr_sexpr(*init))
        }
        other => panic!("unexpected statement: {other:?}"),
    }
}

#[test]
fn let_destructure_desugars_to_one_field_projection_per_item() {
    // `let {field, field: bind} = source` desugars to one ordinary
    // `Stmt::Let` per item, each with its OWN fresh `Expr::Ident(source)`
    // base (not one shared subexpression reused across every
    // `Expr::Field` — the AST must stay a tree). No new `Stmt`/`Expr`
    // variant for this — the ONE bit of extra bookkeeping is a side
    // entry in `ast.destructures` (see the next test), consumed only by
    // types.rs's exhaustiveness check, invisible to every other pass.
    let ast = parse_ok("rule t {\n let {valid, data: d} = p\n}\n");
    let Item::Rule { body, .. } = ast.item(ast.roots[0]) else {
        panic!("expected rule");
    };
    assert_eq!(body.len(), 2, "expected two statements: {body:?}");
    assert_eq!(let_sexpr(&ast, body[0]), "(let valid (. p valid))");
    assert_eq!(let_sexpr(&ast, body[1]), "(let d (. p data))");
}

#[test]
fn let_destructure_records_named_fields_and_rest_flag() {
    let ast = parse_ok("rule t {\n let {valid, data: d} = p\n}\n");
    assert_eq!(
        ast.destructures.len(),
        1,
        "expected one destructure group: {ast:?}"
    );
    let group = &ast.destructures[0];
    assert_eq!(
        group
            .named_fields
            .iter()
            .map(|n| n.text.as_str())
            .collect::<Vec<_>>(),
        vec!["valid", "data"]
    );
    assert!(!group.has_rest);

    let ast = parse_ok("rule t {\n let {valid, ..} = p\n}\n");
    assert_eq!(ast.destructures.len(), 1);
    assert!(ast.destructures[0].has_rest);
}

#[test]
fn let_destructure_rest_must_be_the_last_item() {
    let src = "rule t {\n let {.., valid} = p\n}\n";
    let (tokens, lex_errors) = lexer::lex(src);
    assert!(lex_errors.is_empty(), "lex errors: {lex_errors:?}");
    let (_ast, errors) = parser::parse(src, &tokens);
    assert!(
        errors.iter().any(|e| e.message.contains("last item")),
        "expected a `..`-position rejection, got: {errors:?}"
    );
}

#[test]
fn let_destructure_source_must_be_a_plain_reference() {
    // A call source would desugar to one re-evaluation of the call per
    // destructured field, silently duplicating whatever the callee does
    // -- rejected syntactically rather than chasing which call shapes
    // are actually safe to duplicate.
    let src = "rule t {\n let {valid, data} = Make()\n}\n";
    let (tokens, lex_errors) = lexer::lex(src);
    assert!(lex_errors.is_empty(), "lex errors: {lex_errors:?}");
    let (_ast, errors) = parser::parse(src, &tokens);
    assert!(
        errors.iter().any(|e| e.message.contains("plain reference")),
        "expected a destructuring-source rejection, got: {errors:?}"
    );
}

#[test]
fn struct_update_parses_a_trailing_spread() {
    assert_eq!(
        stmt_sexpr("x := Pair{ data: 5, ..old }"),
        "(:= x (struct Pair (data 5) (.. old)))"
    );
    // Spread-only (no explicit fields) is legal too -- equivalent to
    // `old` itself, modulo the (unchecked) struct name.
    assert_eq!(
        stmt_sexpr("x := Pair{ ..old }"),
        "(:= x (struct Pair (.. old)))"
    );
}

#[test]
fn struct_update_spread_must_be_last_and_a_plain_reference() {
    let call_after = "rule t {\n x := Pair{ data: 5, ..Make() }\n}\n";
    let (tokens, lex_errors) = lexer::lex(call_after);
    assert!(lex_errors.is_empty(), "lex errors: {lex_errors:?}");
    let (_ast, errors) = parser::parse(call_after, &tokens);
    assert!(
        errors.iter().any(|e| e.message.contains("plain reference")),
        "expected a spread-source rejection, got: {errors:?}"
    );

    // A field AFTER `..base` (not just a non-reference base) is
    // rejected the same way -- `..` must be the LAST item.
    let field_after = "rule t {\n x := Pair{ ..old, data: 5 }\n}\n";
    let (tokens, lex_errors) = lexer::lex(field_after);
    assert!(lex_errors.is_empty(), "lex errors: {lex_errors:?}");
    let (_ast, errors) = parser::parse(field_after, &tokens);
    assert!(
        errors.iter().any(|e| e.message.contains("LAST item")),
        "expected a trailing-spread-position rejection, got: {errors:?}"
    );
}

/// `a >>.! b`'s own AST shape is IDENTICAL to plain `a >> b` — `.!`
/// changes nothing about the `Expr::Binary` node itself, only whether
/// its id lands in `ast.lossy` (see ast.rs's own doc comment on that
/// field: a side list, not a new shape, matching `destructures`' own
/// rationale). Same "no new Expr variant" idiom, checked directly here
/// rather than through `stmt_sexpr` (which only returns the s-expr
/// string, throwing away the `Ast` `.lossy` itself lives on).
#[test]
fn lossy_operator_suffix_marks_the_binary_exprs_own_id_not_a_new_shape() {
    let ast = parse_ok("rule t {\n x := a >>.! b\n}\n");
    let Item::Rule { body, .. } = ast.item(ast.roots[0]) else {
        panic!("expected rule");
    };
    let trace::ast::Stmt::Assign { rhs, .. } = ast.stmt(body[0]) else {
        panic!("expected an assignment");
    };
    assert_eq!(ast.expr_sexpr(*rhs), "(>> a b)");
    assert_eq!(ast.lossy.len(), 1, "expected exactly one marked expr");
    assert!(ast.lossy.contains(rhs));

    // The SAME shape, no `.!`, marks nothing.
    let ast = parse_ok("rule t {\n x := a >> b\n}\n");
    assert!(ast.lossy.is_empty());
}

/// The marker isn't shift-specific — it parses after every binary
/// operator the Pratt loop's `infix_bp` table knows about, `+` included.
#[test]
fn lossy_operator_suffix_works_on_every_binary_operator_not_just_shifts() {
    let ast = parse_ok("rule t {\n x := a +.! b\n}\n");
    let Item::Rule { body, .. } = ast.item(ast.roots[0]) else {
        panic!("expected rule");
    };
    let trace::ast::Stmt::Assign { rhs, .. } = ast.stmt(body[0]) else {
        panic!("expected an assignment");
    };
    assert_eq!(ast.expr_sexpr(*rhs), "(+ a b)");
    assert!(ast.lossy.contains(rhs));
}
