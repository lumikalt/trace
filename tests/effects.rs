use trace::ast::{Ast, Item};
use trace::effects::{EffectError, Effects, check};
use trace::{lexer, parser, resolve};

fn run(src: &str) -> (Ast, Effects, Vec<EffectError>) {
    let (tokens, lex_errors) = lexer::lex(src);
    assert!(lex_errors.is_empty(), "lex errors: {lex_errors:?}");
    let (ast, parse_errors) = parser::parse(src, &tokens);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let (res, resolve_errors) = resolve::resolve(&ast);
    assert!(
        resolve_errors.is_empty(),
        "resolve errors: {resolve_errors:?}"
    );
    let (fx, errors) = check(&ast, &res);
    (ast, fx, errors)
}

fn run_ok(src: &str) -> (Ast, Effects) {
    let (ast, fx, errors) = run(src);
    assert!(errors.is_empty(), "effect errors: {errors:?}");
    (ast, fx)
}

/// Find the item id of the fn or rule with the given name.
fn item_named(ast: &Ast, wanted: &str) -> trace::ast::ItemId {
    for (i, item) in ast.items.iter().enumerate() {
        let name = match item {
            Item::Rule { name, .. } | Item::Fn { name, .. } => &name.text,
            _ => continue,
        };
        if name == wanted {
            return trace::ast::ItemId(i as u32);
        }
    }
    panic!("no item named {wanted}");
}

#[test]
fn tick_requires_sequences() {
    let (_, _, errors) = run("rule t {\n tick\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("`tick` requires `<sequences>`"));

    run_ok("rule t <sequences> {\n tick\n}\n");
}

#[test]
fn return_is_rejected_inside_a_rule() {
    // A `rule` has no return value -- found while auditing `_ => {}`
    // wildcard matches for the Assign-vs-Let bug class: nothing
    // downstream had a case for `Stmt::Return` at a rule's top level, so
    // `return f.Deq[]` used to compile clean and silently drop the whole
    // statement (the fifo op, and anything else it wrapped) with zero
    // emitted logic and zero error.
    let (_, _, errors) = run("fifo f : [8]\nrule r {\n return f.Deq[]\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("`return` is only valid inside a function body")
    );

    // Still legal inside a real fn/spawn-callee body.
    run_ok("F(x : [8]) : [8] <combines> {\n return x\n}\n");
}

#[test]
fn while_needs_sequences_or_elaborates() {
    // DESIGN.md's E012 example. `logic` discharges the comparison to a
    // plain [1] condition (a bare comparison no longer types as [1] at
    // all, see TODO.md's comparisons-as-fallible design) so this is
    // still exactly one error, the one this test is actually about.
    let (_, _, errors) =
        run("Bad(x : [8]) : [8] <combines> {\n while logic x <> 0 { x := x >> 1 }\n return x\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("one iteration per cycle"));

    run_ok("Ok(x : [8]) : [8] <sequences> {\n while logic x <> 0 { x := x >> 1 }\n return x\n}\n");
}

#[test]
fn any_requires_chooses() {
    let (_, _, errors) = run("F(x : [4]) : [2] <combines> {\n return any(0..3)\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("`any`"));
    assert!(errors[0].message.contains("<chooses>"));
}

#[test]
fn chooses_only_on_specs() {
    let (_, _, errors) = run("F(x : [4]) : [2] <combines, chooses> {\n return any(0..3)\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("only a `spec`"));

    run_ok("spec S(x : [4]) : [2] <combines, chooses> {\n return any(0..3)\n}\n");
}

#[test]
fn contradictory_colors() {
    let (_, _, errors) = run("F(x : [1]) : [1] <combines, sequences> {\n return x\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("contradicts"));
}

#[test]
fn unknown_effect_name() {
    let (_, _, errors) = run("F(x : [1]) : [1] <transacts> {\n return x\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("unknown effect `transacts`"));
}

#[test]
fn calling_sequences_needs_sequences() {
    let src = "\
Slow(x : [8]) : [8] <sequences> {
    tick
    return x
}

rule r {
    let y = Slow(1)
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("calling `<sequences>` function `Slow`")
    );
}

#[test]
fn specs_are_not_callable() {
    let src = "\
spec S(x : [1]) : [1] <combines, chooses> {
    return any(0..1)
}

rule r {
    let y = S(1)
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("verification-only"));
}

#[test]
fn fails_infers_through_calls() {
    // Full-Verse gating: EVERY item whose computed `fails` is true
    // must itself declare `<fails>`, all the way up a propagating call
    // chain -- `Wrap` calls `Classify` (which fails) and does nothing
    // to catch that, so `Wrap` also fails and also needs `<fails>`.
    // Inference itself is unaffected: `sig.fails` still ends up true
    // for both regardless of what's declared, same as before this
    // gate existed -- only the DECLARATION requirement is new.
    let src = "\
Classify(x : [8]) : [8] <combines, fails> {
    (x <> 0)?
    return x
}

Wrap(x : [8]) : [8] <combines, fails> {
    return Classify(x)
}
";
    let (ast, fx) = run_ok(src);
    assert!(fx.sigs[&item_named(&ast, "Classify")].fails);
    assert!(
        fx.sigs[&item_named(&ast, "Wrap")].fails,
        "fails must propagate through the call"
    );
}

#[test]
fn fails_must_be_declared_wherever_it_ends_up_true() {
    // Explicit `?`, undeclared `<fails>`: an error, matching Verse's
    // own "unhandled failure" (a bare failing expression outside a
    // `<decides>` context does not compile there either).
    let (_, _, errors) = run("Classify(x : [8]) : [8] <combines> {\n (x <> 0)?\n return x\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("does not declare `<fails>`"));

    // The implicit (bare, no `?`) case gets the identical requirement.
    let (_, _, errors) = run("Classify(x : [8]) : [8] <combines> {\n x <> 0\n return x\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("does not declare `<fails>`"));

    // A fifo op, same requirement -- one of the three constructs
    // DESIGN.md's own `fails` section lists side by side with a guard.
    let (_, _, errors) =
        run("module M {\n fifo f : [8]\n Drain() : [8] <combines> {\n return f.Deq[]\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("does not declare `<fails>`"));

    // Calling a `<fails>` function and letting its failure propagate,
    // without declaring `<fails>` on the CALLER too -- the third
    // construct, and the one with the widest blast radius (every
    // propagating caller up the chain needs it, not just the site
    // that directly uses `?`/a fifo op).
    let (_, _, errors) = run(
        "Classify(x : [8]) : [8] <combines, fails> {\n (x <> 0)?\n return x\n}\n\
         Wrap(x : [8]) : [8] <combines> {\n return Classify(x)\n}\n",
    );
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("`Wrap`"));
    assert!(errors[0].message.contains("does not declare `<fails>`"));

    // Declaring `<fails>` is exactly what's needed to make any of the
    // above legal.
    run_ok("Classify(x : [8]) : [8] <combines, fails> {\n (x <> 0)?\n return x\n}\n");
    run_ok("Classify(x : [8]) : [8] <combines, fails> {\n x <> 0\n return x\n}\n");
    run_ok(
        "module M {\n fifo f : [8]\n Drain() : [8] <combines, fails> {\n return f.Deq[]\n }\n}\n",
    );

    // A rule needs no declaration at all -- always a failure context,
    // for any of the three constructs, including calling a `<fails>`
    // function directly.
    run_ok("module M {\n fifo f : [8]\n rule r {\n let x = f.Deq[]\n x <> 0\n }\n}\n");
    run_ok(
        "Classify(x : [8]) : [8] <combines, fails> {\n (x <> 0)?\n return x\n}\n\
         module M {\n out result : [8] = 0\n rule r {\n result := Classify(1)\n }\n}\n",
    );
}

#[test]
fn a_comparison_inside_a_logic_wrapped_calls_argument_still_needs_fails_declared() {
    // Advisor-caught before commit: `logic <call>` discharges only the
    // CALLEE's own fail condition, not an independent comparison nested
    // in the call's arguments -- those are ordinary caller-side
    // expressions, unrelated to what `logic` is discharging. The first
    // pass at `infer_expr`'s `Expr::Logic` arm computed the whole
    // wrapped call's effect in isolation (to capture the callee's own
    // `reads`), which silently swallowed the argument comparison's
    // `fails` along with it -- so a fn wrapping `logic Check(a > b)`
    // could get away with `<combines>` alone, no `<fails>`, exactly the
    // inconsistency `fails_must_be_declared_wherever_it_ends_up_true`
    // above exists to catch for the simpler cases.
    let (_, _, errors) = run(
        "Check(x : [8]) : [8] <combines, fails> {\n (x <> 0)?\n return x\n}\n\
         Wrap(a : [8], b : [8]) : [8] <combines> {\n return logic Check(a > b)\n}\n",
    );
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("`Wrap`"));
    assert!(errors[0].message.contains("does not declare `<fails>`"));

    // Contrast: `logic`-wrapping the COMPARISON directly (not a call
    // argument) still discharges it fully, same as ever -- this fix
    // only narrows the isolation for the call-argument shape above, not
    // the ordinary comparison case.
    run_ok("Wrap(a : [8], b : [8]) : [1] <combines> {\n return logic a > b\n}\n");

    // The primary property the rewrite must preserve: `logic <call>`
    // still discharges the CALLEE's own fails at a fn boundary, same as
    // before this fix (no argument comparison in this one to worry
    // about).
    run_ok(
        "Check(x : [8]) : [8] <combines, fails> {\n (x <> 0)?\n return x\n}\n\
         Wrap(a : [8]) : [1] <combines> {\n return logic Check(a)\n}\n",
    );
}

#[test]
fn fails_must_be_declared_on_an_impl_too() {
    // `impl` shares the same `Item::Fn` shape as `fn`/`spec` under the
    // hood -- confirms the declaration requirement isn't accidentally
    // fn-only.
    let src = "\
spec AnyNonZero(x : [8]) : [8] <combines, chooses> {
    return x
}

impl PickNonZero(x : [8]) : [8] <combines>
    refines AnyNonZero
{
    (x <> 0)?
    return x
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("`PickNonZero`"));
    assert!(errors[0].message.contains("does not declare `<fails>`"));

    let src_ok = "\
spec AnyNonZero(x : [8]) : [8] <combines, chooses> {
    return x
}

impl PickNonZero(x : [8]) : [8] <combines, fails>
    refines AnyNonZero
{
    (x <> 0)?
    return x
}
";
    run_ok(src_ok);
}

#[test]
fn fifo_ops_infer_fails_and_rows() {
    let src = "\
module M {
    fifo input : [8]
    reg count : [8] = 0

    rule drain {
        let x = input.Deq[]
        count := count - 1
    }
}
";
    let (ast, fx) = run_ok(src);
    let sig = &fx.sigs[&item_named(&ast, "drain")];
    assert!(sig.fails, "fifo op makes the rule fallible");
    assert_eq!(sig.reads.len(), 2, "input + count");
    assert_eq!(sig.writes.len(), 2, "input + count");
}

#[test]
fn row_understatement_is_an_error() {
    let src = "\
module M {
    reg pc : [8] = 0
    mem m : [8][256]

    rule r <reads {pc}> {
        let x = m[pc]
    }
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("reads `m`"));

    // Overstating is allowed: conservative rows are sound.
    let src = "\
module M {
    reg pc : [8] = 0
    mem m : [8][256]

    rule r <reads {pc, m}, writes {pc, m}> {
        pc := pc + 1
    }
}
";
    let (_, _, errors) = run(src);
    assert!(
        errors.is_empty(),
        "overstating rows must be legal: {errors:?}"
    );
}

#[test]
fn recursion_requires_elaborates() {
    let (_, _, errors) = run("F(x : [8]) : [8] <combines> {\n return F(x)\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("recursive"));
    assert!(errors[0].message.contains("<elaborates>"));

    // AdderTree-style elaboration recursion is legal.
    run_ok("G(x : [8]) : [8] <elaborates> {\n if x = 0 { return 0 }\n return G(x - 1)\n}\n");
}

#[test]
fn guards_forbidden_at_elaboration_time() {
    // In an initializer.
    let (_, _, errors) = run("module M {\n reg a : [8] = 0\n reg b : [8] = a?\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("elaboration time"));

    // In an <elaborates> body.
    let (_, _, errors) = run("H(x : [8]) : [8] <elaborates> {\n (x <> 0)?\n return x\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("elaboration time"));
}

#[test]
fn implicit_guards_forbidden_at_elaboration_time_too() {
    // A bare (no `?`) condition implicitly guards, same as an explicit
    // `expr?` -- and is rejected identically in an elaboration-time
    // body, with exactly one error (not two: the explicit-Guard arm
    // must not ALSO fire for this).
    let (_, _, errors) = run("H(x : [8]) : [8] <elaborates> {\n x <> 0\n return x\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("elaboration time"));
}

#[test]
fn spawn_callee_must_itself_be_sequences() {
    let src = "\
Fast(x : [8]) : [8] <combines> {
    return x
}

rule r <sequences> {
    let h = spawn Fast(1)
    tick
}
";
    let (_, _, errors) = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("must itself be declared"));

    // A <sequences> callee is fine.
    run_ok(
        "Slow(x : [8]) : [8] <sequences> {\n tick\n return x\n}\n\n\
         rule r <sequences> {\n let h = spawn Slow(1)\n tick\n}\n",
    );
}

#[test]
fn spawn_needs_a_direct_call() {
    let src = "\
rule r <sequences> {
    let h = spawn 1
    tick
}
";
    let (_, _, errors) = run(src);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("direct function call"))
    );
}

#[test]
fn bracket_builtin_call_gets_the_same_checks_as_a_paren_call() {
    // `sync`/`race` are called with brackets (`sync[...]`), a distinct AST
    // shape (`Expr::Bracket`) from an ordinary paren call (`Expr::Call`) —
    // the `<sequences>`-required check has to run for that shape too, not
    // just the paren-call one, or `sync[...]` in an ordinary `combines`
    // rule would silently pass.
    let (_, _, errors) = run("module M {\n reg h : [1] = 0\n rule t {\n let w = sync[h]\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("`sync` requires `<sequences>`"));

    let (_, _, errors) = run("module M {\n reg h : [1] = 0\n rule t {\n let w = race[h]\n }\n}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("`race` requires `<sequences>`"));
}

#[test]
fn all_examples_pass_effect_check() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples");
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "tr") {
            let src = std::fs::read_to_string(&path).unwrap();
            let (_, _, errors) = run(&src);
            assert!(errors.is_empty(), "effect errors in {path:?}: {errors:?}");
        }
    }
}

/// A BARE comparison directly as an `if`'s own condition is branch-
/// scoped and discharged (`infer_stmt`'s `Stmt::If` arm, effects.rs) --
/// mirrors `logic <comparison>`'s existing discharge, but needs no
/// `logic` at all here. Exercised at a FN BOUNDARY specifically (not
/// just a rule) since that's where an incorrectly-propagated `fails`
/// would force a spurious `<fails>` declaration: a `<combines>` callee
/// whose ONLY comparison sits in an if-condition must NOT need `<fails>`
/// at all.
#[test]
fn a_bare_comparison_ifs_condition_does_not_require_fails_declared() {
    run_ok(
        "Classify(a : [8], b : [8]) : [8] <combines> {\n if a > b {\n return 1\n } else {\n \
         return 2\n }\n}\n",
    );
    // No-else shape too -- the discharge doesn't depend on whether an
    // `else` is present, matching this feature's uniform branch-scoping.
    run_ok(
        "Bump(x : [8], y : [8]) : [8] <combines> {\n if x > y {\n return x\n }\n \
         return y\n}\n",
    );
}

/// Contrast: a REAL guard elsewhere in the same body (unrelated to the
/// if-condition comparison) still needs `<fails>` declared -- the
/// if-condition discharge is scoped to exactly that one comparison, not
/// a blanket exemption that could hide an unrelated undeclared failure.
#[test]
fn a_bare_comparison_if_condition_does_not_mask_an_unrelated_guard_needing_fails() {
    let (_, _, errors) = run(
        "Check(a : [8], b : [8], c : [8]) : [8] <combines> {\n if a > b {\n return 1\n \
         }\n (c <> 0)?\n return 2\n}\n",
    );
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("`Check`"));
    assert!(errors[0].message.contains("does not declare `<fails>`"));
}
