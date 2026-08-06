//! Unit-level tests for `closures.rs`'s own pass: `plan`'s error
//! collection and its edits applied via `elaborate::render`, independent
//! of the full firrtl emission pipeline (see tests/sim.rs's
//! `closure_let_runs_through_real_ports` for the real-hardware proof of
//! the happy path).

use trace::closures::{ClosureError, plan};
use trace::elaborate::render;
use trace::{lexer, parser, resolve};

fn run(src: &str) -> (Vec<(trace::lexer::Span, String)>, Vec<ClosureError>) {
    let (tokens, lex_errors) = lexer::lex(src);
    assert!(lex_errors.is_empty(), "lex errors: {lex_errors:?}");
    let (ast, parse_errors) = parser::parse(src, &tokens);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let (res, resolve_errors) = resolve::resolve(&ast);
    assert!(
        resolve_errors.is_empty(),
        "resolve errors: {resolve_errors:?}"
    );
    plan(&ast, &res, src)
}

#[test]
fn a_call_site_splices_the_body_with_the_argument_substituted_for_the_wildcard() {
    let src = "Add(a : [8], b : [8]) : [8] <combines> {\n\
                   return a + b\n\
               }\n\
               module M {\n\
                   in x : [8]\n\
                   out r : [8] = 0\n\
                   rule go {\n\
                       let f = Add(_, 5)\n\
                       r := f(x)\n\
                   }\n\
               }\n";
    let (edits, errors) = run(src);
    assert!(errors.is_empty(), "closure errors: {errors:?}");
    let rendered = render(src, &edits);
    assert!(
        rendered.contains("r := (Add(x, 5))"),
        "unexpected rendering:\n{rendered}"
    );
    assert!(
        !rendered.contains("let f"),
        "the closure's own `let` should be erased entirely:\n{rendered}"
    );
}

#[test]
fn a_bare_read_splices_the_body_verbatim_with_the_wildcard_intact() {
    // The shape DESIGN.md's own `xs.map(f)` example needs: a bare read of
    // a closure-local, not a call, splices `_` intact so a LATER pass
    // (elaborate.rs's own `map` builtin) still sees a real placeholder.
    let src = "Double(x : [32]) : [32] <combines> {\n\
                   return x + x\n\
               }\n\
               AdderTree(xs : list[32]) : [32] <elaborates> {\n\
                   if len(xs) = 1 { return xs[0] }\n\
                   let mid = len(xs) / 2\n\
                   return AdderTree(xs[..mid]) + AdderTree(xs[mid..])\n\
               }\n\
               DoubledSum(xs : list[32]) : [32] <elaborates> {\n\
                   let f = Double(_)\n\
                   return AdderTree(xs.map(f))\n\
               }\n\
               module M {\n\
                   in a : [32]\n\
                   out r : [32] = 0\n\
                   rule go {\n\
                       r := DoubledSum([a])\n\
                   }\n\
               }\n";
    let (edits, errors) = run(src);
    assert!(errors.is_empty(), "closure errors: {errors:?}");
    let rendered = render(src, &edits);
    assert!(
        rendered.contains("AdderTree(xs.map((Double(_))))"),
        "unexpected rendering:\n{rendered}"
    );
}

#[test]
fn calling_a_closure_with_the_wrong_number_of_arguments_is_a_clean_arity_error() {
    let src = "Add(a : [8], b : [8]) : [8] <combines> {\n\
                   return a + b\n\
               }\n\
               module M {\n\
                   in x : [8]\n\
                   out r : [8] = 0\n\
                   rule go {\n\
                       let f = Add(_, 5)\n\
                       r := f(x, x)\n\
                   }\n\
               }\n";
    let (_, errors) = run(src);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].message.contains("takes 1 argument"),
        "{:?}",
        errors[0].message
    );
}
