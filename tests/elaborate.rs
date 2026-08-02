//! Unit-level tests for `elaborate.rs`'s own pass: `plan`'s error
//! collection and `render`'s splice, independent of the full firrtl
//! emission pipeline (see tests/sim.rs's `adder_tree_runs_through_real_
//! ports` for the real-hardware proof of the happy path).

use trace::elaborate::{ElabError, plan, render};
use trace::{effects, lexer, parser, resolve};

fn run(src: &str) -> (Vec<(trace::lexer::Span, String)>, Vec<ElabError>) {
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
    plan(&ast, &res, &fx, src)
}

#[test]
fn adder_tree_reduces_to_a_left_associated_sum_at_each_call_site() {
    let src = "AdderTree(xs: list[bits[32]]) : bits[32] <elaborates> {\n\
                   if len(xs) = 1 { return xs[0] }\n\
                   mid := len(xs) / 2\n\
                   return AdderTree(xs[..mid]) + AdderTree(xs[mid..])\n\
               }\n\
               module M {\n\
                   in a : bits[32]\n\
                   in b : bits[32]\n\
                   in c : bits[32]\n\
                   in d : bits[32]\n\
                   out total : bits[32] = 0\n\
                   rule go {\n\
                       total := AdderTree([a, b, c, d])\n\
                   }\n\
               }\n";
    let (edits, errors) = run(src);
    assert!(errors.is_empty(), "elab errors: {errors:?}");
    assert_eq!(edits.len(), 1, "exactly one top-level call site: {edits:?}");
    let rendered = render(src, &edits);
    assert!(
        rendered.contains("total := ((a + b) + (c + d))"),
        "unexpected rendering:\n{rendered}"
    );
}

#[test]
fn an_odd_length_list_exercises_the_asymmetric_split() {
    // The 4-element case (4->2->1) never exercises `mid := len(xs) / 2`
    // truncating on an odd length -- 3 elements split [a] + [b, c], a
    // genuinely different shape than the power-of-two case.
    let src = "AdderTree(xs: list[bits[32]]) : bits[32] <elaborates> {\n\
                   if len(xs) = 1 { return xs[0] }\n\
                   mid := len(xs) / 2\n\
                   return AdderTree(xs[..mid]) + AdderTree(xs[mid..])\n\
               }\n\
               module M {\n\
                   in a : bits[32]\n\
                   in b : bits[32]\n\
                   in c : bits[32]\n\
                   out total : bits[32] = 0\n\
                   rule go {\n\
                       total := AdderTree([a, b, c])\n\
                   }\n\
               }\n";
    let (edits, errors) = run(src);
    assert!(errors.is_empty(), "elab errors: {errors:?}");
    let rendered = render(src, &edits);
    assert!(
        rendered.contains("total := (a + (b + c))"),
        "unexpected rendering:\n{rendered}"
    );
}

#[test]
fn a_single_element_list_reduces_to_that_element_with_no_addition() {
    let src = "One(xs: list[bits[8]]) : bits[8] <elaborates> {\n\
                   return xs[0]\n\
               }\n\
               module M {\n\
                   in a : bits[8]\n\
                   out total : bits[8] = 0\n\
                   rule go {\n\
                       total := One([a])\n\
                   }\n\
               }\n";
    let (edits, errors) = run(src);
    assert!(errors.is_empty(), "elab errors: {errors:?}");
    let rendered = render(src, &edits);
    assert!(
        rendered.contains("total := a"),
        "unexpected rendering:\n{rendered}"
    );
}

#[test]
fn an_out_of_bounds_list_slice_is_a_compile_error_not_a_panic() {
    let src = "Bad(xs: list[bits[8]]) : bits[8] <elaborates> {\n\
                   return xs[5]\n\
               }\n\
               module M {\n\
                   in a : bits[8]\n\
                   out total : bits[8] = 0\n\
                   rule go {\n\
                       total := Bad([a])\n\
                   }\n\
               }\n";
    let (_edits, errors) = run(src);
    assert!(
        errors.iter().any(|e| e.message.contains("out of bounds")),
        "expected an out-of-bounds error, got: {errors:?}"
    );
}

#[test]
fn a_non_terminating_elaborates_function_hits_the_depth_cap_instead_of_hanging() {
    let src = "Loopy(x: bits[8]) : bits[8] <elaborates> {\n\
                   return Loopy(x)\n\
               }\n\
               module M {\n\
                   in a : bits[8]\n\
                   out total : bits[8] = 0\n\
                   rule go {\n\
                       total := Loopy(a)\n\
                   }\n\
               }\n";
    let (_edits, errors) = run(src);
    assert!(
        errors.iter().any(|e| e.message.contains("depth limit")),
        "expected a depth-limit error, got: {errors:?}"
    );
}

#[test]
fn an_elaborates_call_composes_with_an_unrelated_state_write_in_the_same_rule() {
    // `elaborate.rs`'s own `sig.writes`-not-empty rejection (an
    // `<elaborates>` function that writes state) is unreachable through
    // any effects.rs-legal program: effects.rs already rejects a state
    // write in an elaboration position outright, before `elaborate.rs`
    // ever runs. This just proves an elaborates call and an ordinary
    // state write can share a rule without interfering.
    let src = "Id(x: bits[8]) : bits[8] <elaborates> {\n\
                   return x\n\
               }\n\
               module M {\n\
                   reg r : bits[8] = 0\n\
                   in a : bits[8]\n\
                   out total : bits[8] = 0\n\
                   rule go {\n\
                       r := a\n\
                       total := Id(a)\n\
                   }\n\
               }\n";
    let (edits, errors) = run(src);
    assert!(errors.is_empty(), "elab errors: {errors:?}");
    assert_eq!(edits.len(), 1);
}
