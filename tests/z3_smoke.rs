//! Spike/canary for the SMT backend (DESIGN.md's "Toward a dependent/
//! refinement type system (SMT-backed, planned)"). Confirms the `z3`
//! crate links against `pkgs.z3` (devenv.nix) and can decide a trivial
//! bitvector query -- if this ever fails to compile/link/run, every
//! later stage of that plan is blocked on the same root cause, so this
//! stays in the suite as an early, cheap signal rather than only ever
//! being discovered inside a much bigger encoder.

use z3::ast::BV;
use z3::{SatResult, Solver};

#[test]
fn z3_proves_x_plus_one_is_never_equal_to_x() {
    let solver = Solver::new();

    let x = BV::new_const("x", 8);
    let one = BV::from_u64(1, 8);

    // Assert the NEGATION of what we want to prove (x + 1 == x, mod
    // 2^8) and check it's UNSAT -- the standard "prove by refuting the
    // counterexample" shape every later obligation query will use.
    solver.assert(x.bvadd(&one).eq(&x));
    assert_eq!(solver.check(), SatResult::Unsat);
}

#[test]
fn z3_finds_a_real_counterexample_when_a_claim_is_false() {
    let solver = Solver::new();

    let x = BV::new_const("x", 8);
    let ten = BV::from_u64(10, 8);

    // A FALSE claim (x is always < 10) should be refutable -- SAT, with
    // a model giving a concrete violating value, exactly the
    // diagnostic upgrade DESIGN.md's plan describes.
    solver.assert(x.bvuge(&ten));
    assert_eq!(solver.check(), SatResult::Sat);
    let model = solver.get_model().expect("sat query has a model");
    let value = model
        .eval(&x, true)
        .and_then(|v| v.as_u64())
        .expect("model assigns x a concrete value");
    assert!(value >= 10);
}
