use trace::bounds::{self, BoundsError};
use trace::{effects, lexer, parser, resolve, types};

fn run(src: &str) -> Vec<BoundsError> {
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
    let (ty, type_errors) = types::check(&ast, &res, &fx);
    assert!(type_errors.is_empty(), "type errors: {type_errors:?}");
    let (_, errors) = bounds::check(&ast, &res, &fx, &ty);
    errors
}

#[test]
fn guarded_increment_is_proven() {
    // The motivating pattern: `i`'s declared bound (`< 9`) is exactly
    // matched by the composed bound of `i + 1` under the `if i < 8`
    // narrowing (8 + 2 - 1 = 9), and the `else` branch's `i := 0`
    // trivially satisfies it.
    let src = "\
module M {
    reg i : [4] where i < 9 = 0
    rule bump {
        if i < 8 {
            i := i + 1
        } else {
            i := 0
        }
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn unguarded_increment_is_rejected() {
    // No narrowing at all: `i`'s frozen bound stays `< 9` for the whole
    // body, so `i + 1` composes to `< 10`, exceeding the declared bound.
    let src = "\
module M {
    reg i : [4] where i < 9 = 0
    rule bump {
        i := i + 1
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot verify"));
}

#[test]
fn insufficiently_narrowed_guard_is_rejected() {
    // `if i < 9` doesn't narrow enough: the composed bound (9 + 2 - 1 =
    // 10) still exceeds the declared `< 9`. A regression here would
    // mean the narrowing accepted a guard that isn't actually tight
    // enough.
    let src = "\
module M {
    reg i : [4] where i < 9 = 0
    rule bump {
        if i < 9 {
            i := i + 1
        }
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot verify"));
}

#[test]
fn subtraction_by_a_provably_safe_amount_is_accepted() {
    // v7: `Sub` now composes -- `i - 0` is trivially in-bounds (the
    // subtrahend's range `[0,1)` can never exceed the minuend's own
    // lower bound `0`), and needs no guard at all to prove.
    let src = "\
module M {
    reg i : [4] where i < 9 = 0
    rule bump {
        i := i - 0
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn unguarded_subtraction_that_could_underflow_is_rejected() {
    // Unlike `i - 0` above: `i`'s frozen lower bound stays at its
    // declared floor (0) with no guard narrowing it, so `i - 1` could
    // underflow when `i == 0` -- must fail closed, not silently accepted.
    let src = "\
module M {
    reg i : [4] where i < 9 = 0
    rule bump {
        i := i - 1
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot verify"));
}

#[test]
fn gt_guarded_subtraction_is_proven() {
    // The actual driving pattern (examples/countdown_bounded.tr): `cnt >
    // 0` narrows the LOWER end to 1 for the `then` branch, making `cnt -
    // 1`'s computed range `[0,99)` -- provably within the declared
    // `[0,100)`. The `else` branch writes a bare literal, needing no
    // narrowing at all.
    let src = "\
module M {
    reg cnt : [8] where cnt < 100 = 0
    rule count {
        if cnt > 0 {
            cnt := cnt - 1
        } else {
            cnt := 99
        }
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn ge_guarded_subtraction_is_proven() {
    // `cnt >= 1` narrows the lower end to `max(current, 1) = 1` -- the
    // same result `Gt`'s `cnt > 0` reaches via a different constant, so
    // this alone doesn't discriminate `Ge`'s exact formula from `Gt`'s
    // (see the dedicated off-by-one test right below for that).
    let src = "\
module M {
    reg cnt : [8] where cnt < 100 = 0
    rule count {
        if cnt >= 1 {
            cnt := cnt - 1
        } else {
            cnt := 99
        }
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn ge_narrows_to_the_constant_itself_not_one_past_it() {
    // Discriminates `Ge`'s formula (`max(current, k)`) from `Gt`'s
    // (`max(current, k+1)`): `cnt >= 0` is trivially true for an
    // unsigned value and must narrow the lower end to `max(0, 0) = 0`
    // -- UNCHANGED from the declared floor -- so `cnt - 1` stays
    // genuinely unprovable (`cnt` could still be 0). If `Ge` incorrectly
    // reused `Gt`'s off-by-one (narrowing to `max(0, 1) = 1` instead),
    // this would wrongly ACCEPT the write instead.
    let src = "\
module M {
    reg cnt : [4] where cnt < 9 = 0
    rule count {
        if cnt >= 0 {
            cnt := cnt - 1
        }
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot verify"));
}

#[test]
fn ne_guarded_subtraction_stays_unprovable() {
    // `<>` guard narrowing is deliberately NOT recognized (this module's
    // own doc comment): sound only when the excluded constant equals
    // the CURRENT frozen bound exactly, a genuinely different argument
    // than a plain inequality. Confirms it doesn't work "by accident"
    // now that `Sub` composes -- the actual `while_countdown.tr` shape
    // stays rejected under a `where` bound.
    let src = "\
module M {
    reg cnt : [8] where cnt < 100 = 0
    rule count <sequences> {
        while cnt <> 0 {
            cnt := cnt - 1
            tick
        }
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot verify"));
}

#[test]
fn multiplication_on_the_rhs_is_rejected_as_unknown() {
    let src = "\
module M {
    reg i : [4] where i < 9 = 0
    rule bump {
        i := i * 1
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot verify"));
}

#[test]
fn write_inside_a_called_fn_is_caught() {
    // This pass walks every `Item::Fn` directly (not through the call
    // graph), so a write hiding inside a callee must still be checked
    // against the SAME frozen entry bound -- confirms it isn't only
    // rule bodies that get walked.
    let src = "\
module M {
    reg i : [4] where i < 9 = 0
    Bump() {
        i := i + 1
    }
    rule bump {
        Bump()
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot verify"));
}

#[test]
fn let_local_carries_a_bound_across_a_following_assign() {
    // The realistic `let next = i + 1; i := next` shape -- the local's
    // OWN computed bound must be tracked and consulted, not just a bare
    // ident/literal RHS.
    let src = "\
module M {
    reg i : [4] where i < 9 = 0
    rule bump {
        if i < 8 {
            let next = i + 1
            i := next
        } else {
            i := 0
        }
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn write_inside_a_sequences_while_body_is_checked() {
    // Each `<sequences>` `while` iteration is its own clock edge
    // (DESIGN.md) -- a write inside the loop body is checked against
    // the SAME frozen entry bound as any other write site, not skipped.
    let src = "\
module M {
    reg i : [4] where i < 9 = 0
    rule bump <sequences> {
        while i < 5 {
            let step = i + 1
            i := step
            tick
        }
    }
}
";
    // 5 + 2 - 1 = 6, well under the declared `< 9` -- passes.
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn write_inside_a_sequences_while_body_that_exceeds_the_bound_is_rejected() {
    let src = "\
module M {
    reg i : [4] where i < 9 = 0
    rule bump <sequences> {
        while i < 9 {
            let step = i + 1
            i := step
            tick
        }
    }
}
";
    // The loop's own guard (`i < 9`) doesn't narrow at all (it already
    // matches the declared bound exactly): 9 + 2 - 1 = 10 exceeds `< 9`.
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot verify"));
}

#[test]
fn unbounded_reg_is_unaffected() {
    // A plain reg with no `where` clause must never be checked at all —
    // confirms the pass is opt-in, not a blanket restriction on every
    // reg's writes.
    let src = "\
module M {
    reg i : [4] = 0
    rule bump {
        i := i + 1
    }
}
";
    assert!(run(src).is_empty());
}

#[test]
fn a_bound_on_a_reg_with_an_unknown_width_is_rejected() {
    // `n` is a plain reg, not an elaboration parameter, so `[n]` isn't
    // const-foldable at type-eval time: `types/eval.rs` resolves it to
    // `Ty::Bits(Width::Unknown)` with no error of its own (an elaboration
    // parameter would resolve fine; this name just never can). Without
    // its own check, this pass would silently drop `i` from `bounded`
    // and prove nothing at all about a bound the user explicitly wrote,
    // with zero diagnostic anywhere in the pipeline.
    let src = "\
module M {
    reg n : [8] = 4
    reg i : [n] where i < 9 = 0
    rule bump {
        if i < 8 {
            i := i + 1
        } else {
            i := 0
        }
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("concretely-known"));
}

#[test]
fn composed_bound_exceeding_the_declared_width_is_rejected_even_under_the_logical_limit() {
    // `i`'s declared LOGICAL bound (`< 20`) is nowhere near violated
    // (the composed value never exceeds 9) -- but `i`'s declared WIDTH
    // is only `[3]` (max representable value 7), so a composed bound of
    // 9 would silently wrap in real hardware before ever reaching the
    // logical limit, invalidating the whole argument. This pins the
    // width-clamp arm specifically, distinct from the plain
    // over-the-logical-limit rejection every other negative test here
    // exercises.
    let src = "\
module M {
    reg i : [3] where i < 20 = 0
    rule bump {
        if i < 7 {
            i := i + 1 + 1
        } else {
            i := 0
        }
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("declared width"));
}

#[test]
fn two_sided_bound_guarded_wraparound_is_proven() {
    // The `schedule.rs` driving example's own `bump_j`-style shape: `j`'s
    // declared range is `[5,10)`, and every write is checked directly
    // against the full declared range on BOTH ends (no condition-based
    // narrowing of the lower end is needed here at all).
    let src = "\
module M {
    reg j : [4] where 5 <= j < 10 = 5
    rule bump {
        if j < 9 {
            j := j + 1
        } else {
            j := 5
        }
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn two_sided_bound_write_below_the_lower_end_is_rejected() {
    let src = "\
module M {
    reg j : [4] where 5 <= j < 10 = 5
    rule bad {
        j := 0
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("go below 5"));
}
