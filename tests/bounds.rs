use trace::bounds::{self, Bounds, BoundsError};
use trace::effects::Effects;
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

/// v16: the `run` helper above discards `Bounds`/`Effects` entirely,
/// which every prior test only ever needed errors from. `site_ranges`
/// tests need both: `Effects`'s own `mem_read_idx`/`mem_write_idx`
/// (populated independently by `effects.rs`, the SAME enumerable set
/// `schedule.rs` itself consults) to locate the exact `ExprId` a mem
/// index sits at, and `Bounds.site_ranges` to check what `bounds.rs`
/// exported for it.
fn run_with_bounds(src: &str) -> (Effects, Bounds, Vec<BoundsError>) {
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
    let (bounds, errors) = bounds::check(&ast, &res, &fx, &ty);
    (fx, bounds, errors)
}

/// The one DISTINCT `ExprId` recorded across every item's own
/// `mem_write_idx`/`mem_read_idx` for `mem` -- panics if there isn't
/// exactly one (keeps each test's driving shape unambiguous rather
/// than silently picking an arbitrary match). The SAME `ExprId` can
/// legitimately appear more than once here -- `effects.rs`'s own
/// aggregation merges a callee's mem-index sites into every caller's
/// own summary too, so a mem read inside a called `fn`'s body shows up
/// under both that `fn`'s own item AND the rule that calls it.
fn the_only_mem_index(fx: &Effects) -> trace::ast::ExprId {
    let mut found = std::collections::BTreeSet::new();
    for sig in fx.sigs.values() {
        for idxs in sig.mem_write_idx.values() {
            found.extend(idxs.iter().copied());
        }
        for idxs in sig.mem_read_idx.values() {
            found.extend(idxs.iter().copied());
        }
    }
    let found: Vec<_> = found.into_iter().collect();
    assert_eq!(
        found.len(),
        1,
        "expected exactly one distinct mem index site, found {found:?}"
    );
    found[0]
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
fn ne_guarded_lower_edge_subtraction_is_proven() {
    // `<> 0` excludes the CURRENT lower bound (0) exactly, narrowing
    // `cnt`'s lower end up to 1 -- sound, since a `where cnt < 100`
    // reg's real floor is 0. This is the `while_countdown.tr` shape
    // (`while cnt <> 0 { cnt := cnt - 1 }`), now provable when `cnt`
    // carries a `where` bound (the real `while_countdown.tr` file still
    // doesn't -- and couldn't usefully gain one, since its `cnt := x`
    // reads an unbounded `in` port every cycle, which stays unprovable
    // regardless of this feature).
    // Discriminates the lower-narrow formula: a buggy `k` instead of
    // `k + 1` would leave the lower end at 0, and `0 - 1` would still
    // fail.
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
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn ne_guarded_upper_edge_increment_is_proven() {
    // `<> 99` excludes the CURRENT max (`hi - 1 = 99`) exactly,
    // narrowing `cnt`'s upper end down to 99 -- sound, since `cnt`'s
    // declared range is `[0, 100)`. Mirrors `output_bounded_ne.tr`'s own
    // driving example. Discriminates both the upper-narrow formula (a
    // buggy `k + 1` or `k - 1` instead of `k` would compute the wrong
    // new ceiling) and the edge condition itself (`k + 1 == hi`, not
    // `k == hi`).
    let src = "\
module M {
    out cnt : [8] where cnt < 100 = 0
    rule count {
        if cnt <> 99 {
            cnt := cnt + 1
        } else {
            cnt := 0
        }
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn ne_guarded_mid_range_lower_subtraction_stays_unprovable() {
    // `<> 50` excludes neither edge of `cnt`'s declared `[0, 100)` range
    // -- narrowing here would split the range into two disjoint pieces
    // a single interval can't express, so this must stay a no-op.
    // Discriminates an UNSOUND generalization of the lower-narrow rule
    // (e.g. `k >= lo` instead of exact equality `k == lo`): such a bug
    // would wrongly narrow the lower end to 51, and `51 - 50 = 1` would
    // wrongly be accepted. Correctly: no narrow, `0 - 50` underflows,
    // unprovable.
    let src = "\
module M {
    reg cnt : [8] where cnt < 100 = 0
    rule count {
        if cnt <> 50 {
            cnt := cnt - 50
        }
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot verify"));
}

#[test]
fn ne_guarded_mid_range_upper_addition_stays_unprovable() {
    // Same non-edge exclusion as above, the other direction.
    // Discriminates an UNSOUND generalization of the upper-narrow rule
    // (e.g. any `k < hi` instead of exact equality `k + 1 == hi`): such
    // a bug would wrongly narrow the upper end to 50, and `50 + 50 =
    // 100` would wrongly be accepted (`hi` would land exactly at 100,
    // not exceeding it). Correctly: no narrow, `100 + 50 - 1 = 149`
    // exceeds the declared bound, rejected.
    let src = "\
module M {
    reg cnt : [8] where cnt < 100 = 0
    rule count {
        if cnt <> 50 {
            cnt := cnt + 50
        }
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot verify"));
}

#[test]
fn ne_guarded_lower_edge_subtraction_with_nonzero_floor_is_proven() {
    // Same lower-edge narrowing as the `while_countdown.tr`-shaped test
    // above, but with a v6 two-sided bound (`5 <= j < 10`) whose floor
    // is NOT the implicit 0 -- discriminates a bug that reads `k == 0`
    // (a plausible mistake if the implicit-floor case were special-cased
    // instead of comparing against the ACTUAL current `lo`) instead of
    // `k == *lo`, which would fail to narrow here and leave `j - 1`
    // unprovable (`5 - 1` underflows).
    let src = "\
module M {
    reg j : [4] where 5 <= j < 10 = 5
    rule count {
        if j <> 5 {
            j := j - 1
        }
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn ne_commuted_lower_edge_subtraction_is_proven() {
    // `<const> <> <reg>` (the constant on the LEFT) IS recognized for
    // `Ne` (v10) -- narrowing only ever treats a guard as a pass/fail
    // predicate, never a comparison's own returned value (which IS
    // order-sensitive in this language -- see `type_binop`'s "yields
    // the LHS's type/value" rule), and `0 <> cnt` is the same fact as
    // `cnt <> 0` for that purpose. Exact mirror of
    // `ne_guarded_lower_edge_subtraction_is_proven` with the operands
    // swapped.
    let src = "\
module M {
    reg cnt : [8] where cnt < 100 = 0
    rule count {
        if 0 <> cnt {
            cnt := cnt - 1
        }
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn ne_commuted_lower_edge_subtraction_in_while_is_proven() {
    // Same commuted-form narrowing, but reached through `while`'s own
    // condition rather than `if`'s -- confirms the commuted fallback
    // isn't accidentally `if`-only. `Stmt::While` renders through the
    // identical `if`-shaped path `check_cond` uses (see `types/stmt.rs`'s
    // own `Stmt::While` arm, `allow_bare_comparison: true`), so this is
    // expected to work uniformly, and does: exact mirror of
    // `ne_guarded_lower_edge_subtraction_is_proven` (the `while cnt <>
    // 0` shape) with the operands swapped.
    let src = "\
module M {
    reg cnt : [8] where cnt < 100 = 0
    rule count <sequences> {
        while 0 <> cnt {
            cnt := cnt - 1
            tick
        }
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn commuted_lt_is_not_recognized() {
    // Unlike `Ne`, `Lt`'s commuted fallback is NOT added -- `k < x` and
    // `x < k` are different claims even as bare predicates (`8 < i`
    // means `i > 8`, not `i < 8`), so recognizing a constant on the
    // LEFT here would mean recognizing a different operator in the
    // flipped position, a separate feature nobody asked for. Pins that
    // the `Ne`-only commuted fallback in `narrow_for_condition` doesn't
    // leak into the other arms: if it were hoisted out of its `Ne`-only
    // gate, this would wrongly start narrowing `i`'s upper end here.
    let src = "\
module M {
    reg i : [4] where i < 9 = 0
    rule bump {
        if 8 < i {
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
fn multiplication_by_a_literal_is_proven() {
    // `Mul` composes (v11): both operands are non-negative, so a
    // product's extremes correspond exactly to the operands' own
    // extremes -- `i * 1` composes to `lo = 0*1 = 0`, `hi = 8*1+1 = 9`,
    // exactly matching the declared bound.
    let src = "\
module M {
    reg i : [4] where i < 9 = 0
    rule bump {
        i := i * 1
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn scaled_counter_doubling_is_proven() {
    // The driving example's own shape (`examples/scaled_counter.tr`):
    // under `if cnt < 10`, `cnt`'s tracked range narrows to `[0, 10)`,
    // so `a_max = 9`; the literal `2` gives `b_lo = b_max = 2`. `cnt *
    // 2` composes to `lo = 0*2 = 0`, `hi = 9*2+1 = 19` -- provably
    // within the declared `[0, 100)`.
    let src = "\
module M {
    reg cnt : [8] where cnt < 100 = 1
    rule double {
        if cnt < 10 {
            cnt := cnt * 2
        } else {
            cnt := 1
        }
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn unguarded_multiplication_that_could_exceed_the_bound_is_rejected() {
    // No narrowing guard: `cnt`'s full declared range `[0, 100)`
    // composes to `hi = 99*2+1 = 199`, exceeding the bound. Discriminates
    // an UNSOUND bug that computes `hi` from the operands' own `lo`
    // instead of their `max` (e.g. `hi = a_lo*b_lo+1 = 0*2+1 = 1`, which
    // would wrongly accept this write).
    let src = "\
module M {
    reg cnt : [8] where cnt < 100 = 1
    rule double {
        cnt := cnt * 2
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot verify"));
}

#[test]
fn multiplication_composed_bound_exceeding_the_declared_width_is_rejected() {
    // Same width-clamp distinction as the `Add`-based test above, now
    // for `Mul`: `i`'s declared LOGICAL bound (`< 20`) is nowhere near
    // violated (composed value never exceeds 9), but `i`'s declared
    // WIDTH is only `[3]` (max representable value 7) -- a composed
    // bound of 10 would silently wrap before reaching the logical limit.
    let src = "\
module M {
    reg i : [3] where i < 20 = 0
    rule bump {
        if i < 4 {
            i := i * 3
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
fn multiplication_ceiling_is_exclusive_not_inclusive() {
    // `hi` is exclusive everywhere in this module, so the ceiling must
    // be `a_max * b_max + 1`, not `a_max * b_max`. Tight enough to
    // discriminate dropping the `+ 1`: `cnt < 5` narrows to `[0,5)`
    // (`a_max = 4`); the literal `2` gives `b_max = 2`. Correctly,
    // `hi = 4*2+1 = 9`, which exceeds the declared bound (`cnt < 8`),
    // so this must be REJECTED. A buggy version omitting the `+ 1`
    // (`hi = 4*2 = 8`) would land exactly at the declared bound and be
    // wrongly ACCEPTED.
    let src = "\
module M {
    reg cnt : [8] where cnt < 8 = 0
    rule bump {
        if cnt < 5 {
            cnt := cnt * 2
        } else {
            cnt := 0
        }
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot verify"));
}

#[test]
fn multiplication_uses_the_operands_own_max_not_their_exclusive_upper_bound() {
    // Tight enough to discriminate an off-by-one that uses `a_hi`/`b_hi`
    // (the exclusive upper bound) instead of `a_max`/`b_max` (`hi - 1`)
    // for the max computation: correctly, `cnt < 5` narrows to `[0,5)`,
    // `a_max = 4`; the literal `2` gives `b_max = 2`. `hi = 4*2+1 = 9`,
    // and `cnt < 10` is the declared bound, so `9 < 10` is provable. A
    // buggy version using `a_hi*b_hi+1 = 5*3+1 = 16` would wrongly
    // REJECT this write (16 > 10).
    let src = "\
module M {
    reg cnt : [8] where cnt < 10 = 0
    rule bump {
        if cnt < 5 {
            cnt := cnt * 2
        } else {
            cnt := 0
        }
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
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

#[test]
fn out_guarded_increment_is_proven() {
    // v8: `where` extended to `out` -- the identical induction argument
    // `guarded_increment_is_proven` already pins for `reg`, now on an
    // `out` (register-backed, same `Stmt::Assign` shape).
    let src = "\
module M {
    out i : [4] where i < 9 = 0
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
fn out_unguarded_increment_is_rejected() {
    let src = "\
module M {
    out i : [4] where i < 9 = 0
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
fn call_argument_within_the_declared_param_bound_is_proven() {
    // v12: cross-boundary bound propagation. `x`'s own declared range
    // `[0,10)` exactly matches `Bump`'s declared param bound for `i` --
    // `Bump`'s own body (`cnt := i + 1`) is separately proven via the
    // existing per-item induction (unchanged since v5), the param's
    // bound seeded into `self.bounded` exactly like a reg's own.
    let src = "\
module M {
    reg x : [8] where x < 10 = 0
    reg cnt : [8] where cnt < 20 = 0
    Bump(i : [8] where i < 10) {
        cnt := i + 1
    }
    rule step {
        Bump(x)
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn call_argument_exceeding_the_declared_param_bound_is_rejected() {
    // `y`'s own declared range `[0,50)` is NOT provably within `Bump`'s
    // declared param bound `[0,10)` -- the call site is where this
    // arc's first cross-boundary obligation gets checked. Without this
    // check, `Bump`'s own proof (sound only if callers respect `i <
    // 10`) would be silently unenforced.
    let src = "\
module M {
    reg y : [8] where y < 50 = 0
    reg cnt : [8] where cnt < 20 = 0
    Bump(i : [8] where i < 10) {
        cnt := i + 1
    }
    rule step {
        Bump(y)
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("argument for parameter"));
}

#[test]
fn unbounded_param_argument_is_unchecked() {
    // A param with NO `where` bound imposes no obligation at all --
    // `z`'s declared range (`[0,200)`) is irrelevant here, since `i`
    // has nothing to check it against. Confirms no false rejection.
    let src = "\
module M {
    reg z : [8] where z < 200 = 0
    reg w : [4] where w < 9 = 0
    Bump(i : [8]) {
        w := 0
    }
    rule step {
        Bump(z)
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn two_sided_param_bound_is_recognized() {
    // The v6 two-sided surface form (`where L <= i < K`) works
    // identically on a param -- same parse path (`parse_where_bound`),
    // same collection path (`collect_one_bounded_def`). `j`'s own
    // declared range `[5,10)` exactly matches.
    let src = "\
module M {
    reg j : [8] where 5 <= j < 10 = 5
    reg cnt : [8] where cnt < 20 = 0
    Bump(i : [8] where 5 <= i < 10) {
        cnt := i + 1
    }
    rule step {
        Bump(j)
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn return_value_within_the_declared_bound_is_proven() {
    // v13: the mirror of v12 in the OTHER direction. `i + 5` under
    // `i < 10` has range `[5,15)`, well within the declared
    // postcondition `result < 20` -- a NEW checked position
    // (`Stmt::Return`) that didn't exist before this feature.
    let src = "\
module M {
    Bump(i : [8] where i < 10) : [8] where _ < 20 {
        return i + 5
    }
    rule step {
        Bump(3)
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn return_value_exceeding_the_declared_bound_is_rejected() {
    // `i + 100` under `i < 10` can reach 109, violating the declared
    // postcondition `result < 20` -- caught at the `return` statement
    // itself, regardless of whether `BadBump` is ever called (this
    // pass checks every fn's own body unconditionally, matching the
    // existing per-item induction).
    let src = "\
module M {
    BadBump(i : [8] where i < 10) : [8] where _ < 20 {
        return i + 100
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("return value"));
}

#[test]
fn call_result_composes_into_a_bounded_write_via_declared_ret_bound() {
    // The key new-capability proof: before v13, `Expr::Call` always
    // composed to `None`, so `Bump(3) + Bump(4)` would be "unsupported
    // expression shape" regardless of either call's own provable
    // range. With `Bump`'s declared postcondition (`result < 20`)
    // propagated, each call's own range is `[0,20)`, composing to
    // `[0,39]` -- exactly fitting `total`'s own `< 40` bound.
    let src = "\
module M {
    reg total : [8] where total < 40 = 0
    Bump(i : [8] where i < 10) : [8] where _ < 20 {
        return i + 5
    }
    rule step {
        total := Bump(3) + Bump(4)
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn return_bound_composition_uses_the_full_declared_width_not_narrower() {
    // A fencepost-sensitive companion to the test above: `Bump`'s
    // declared postcondition here is `result < 21` (not `< 20`), so the
    // TRUE composed range (`[0,41)`) just barely EXCEEDS `total`'s own
    // `< 40` bound -- correctly rejected. If the propagated upper were
    // ever narrower than what's actually declared (e.g. an off-by-one
    // at the `Expr::Call` arm's own lookup), the composed range would
    // shrink to fit under 40 and this would be wrongly ACCEPTED instead
    // -- caught via bug-reintroduction (temporarily subtracting 1 from
    // the propagated upper flips this from 1 error to 0, confirmed then
    // reverted). Unlike the test above (whose own tight boundary
    // already happens to catch the OTHER, more dangerous over-widening
    // direction), this one specifically pins under-reporting.
    let src = "\
module M {
    reg total : [8] where total < 40 = 0
    Bump(i : [8] where i < 10) : [8] where _ < 21 {
        return i + 5
    }
    rule step {
        total := Bump(3) + Bump(4)
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot verify this write"));
}

#[test]
fn return_bound_with_no_return_statement_is_rejected() {
    // A real soundness hole, caught by advisor before committing: the
    // `fn_ret_bound` entry is created purely from the DECLARATION at
    // collection time, with no coupling to whether any `Stmt::Return`
    // ever actually got checked against it. Without this check, `Bump`
    // here would trust `result < 20` at every call site with ZERO
    // obligations verified -- confirmed empirically (this exact source
    // compiled clean with 0 errors before `check_return_site_
    // exhaustiveness` existed).
    let src = "\
module M {
    reg total : [8] where total < 40 = 0
    Bump(i : [8]) : [8] where _ < 20 {
    }
    rule step {
        total := Bump(3) + Bump(4)
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("never checked against an actual `return` statement")
    );
}

#[test]
fn unbounded_fn_call_result_is_still_unprovable() {
    // A fn with NO declared postcondition still composes to `None` --
    // return-bound propagation is opt-in, not blanket inference. `i`'s
    // own declared range would make `i + 5` provable if `Bump` composed
    // its return value automatically, but it must not.
    let src = "\
module M {
    reg total : [8] where total < 40 = 0
    Bump(i : [8] where i < 10) : [8] {
        return i + 5
    }
    rule step {
        total := Bump(3) + Bump(4)
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot verify this write"));
}

#[test]
fn two_sided_return_bound_is_recognized() {
    // The v6 two-sided surface form (`where L <= _ < K`) works
    // identically on a return bound -- same parse path
    // (`parse_where_bound`), same collection path
    // (`collect_one_ret_bound`).
    let src = "\
module M {
    Bump(i : [8] where 5 <= i < 10) : [8] where 5 <= _ < 15 {
        return i
    }
    rule step {
        Bump(7)
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn call_argument_in_an_if_condition_is_checked() {
    // v14: a call embedded directly in an `if`'s own CONDITION was
    // never checked before this pass -- `narrow_for_condition` only
    // ever pattern-matches `cond`'s shape, never routes it through
    // `expr_bound`. `Bump(50)`'s own argument (50) violates its
    // declared param bound (`i < 10`).
    let src = "\
module M {
    reg total : [8] where total < 40 = 0
    Bump(i : [8] where i < 10) : [8] {
        return i
    }
    rule step {
        if Bump(50) < 5 {
            total := 1
        } else {
            total := 2
        }
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("argument for parameter"));
}

#[test]
fn call_argument_in_a_while_condition_is_checked() {
    // The `while` mirror of the test above.
    let src = "\
module M {
    reg dummy : [8] = 0
    Bump(i : [8] where i < 10) : [8] {
        return i
    }
    rule count <sequences> {
        while Bump(50) < 5 {
            dummy := 1
            tick
        }
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("argument for parameter"));
}

#[test]
fn call_argument_in_an_if_let_init_is_checked() {
    // v14: `if let`'s own `init` (`if let x = Classify(50) { ... }`,
    // the exact shape `examples/if_let_failing_call.tr` already uses)
    // was never checked before this pass either -- `Stmt::IfLet`'s own
    // arm destructured `init` with `..` and never touched it.
    let src = "\
Classify(i : [8] where i < 10) : [8] <combines, fails> {
    (i <> 0)?
    return i
}

module M {
    reg out_reg : [8] = 0
    rule step {
        if let x = Classify(50) {
            out_reg := x
        }
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("argument for parameter"));
}

#[test]
fn call_argument_in_a_while_let_init_is_checked() {
    // The `while let` mirror of the `if let` test above -- its own
    // dedicated `Stmt::WhileLet` call site (`check_calls_in(init, ...)`)
    // had zero coverage otherwise: none of this pass's other tests
    // exercise it, so a wrong expression passed there would go
    // undetected. `init` here is `Expr::Guard(Call(Bump, [50]))` (a
    // `?T`-returning fn's own call, unwrapped with `?` -- the same
    // shape `examples/call_struct_return.tr`'s `Wrap` demonstrates) --
    // `check_calls_in`'s recursion through `Guard`'s own `sub_exprs`
    // child reaches the nested `Call` correctly.
    let src = "\
Bump(i : [8] where i < 10) : ?[8] <combines, fails> {
    (i <> 200)?
    return i
}

module M {
    reg dummy : [8] = 0
    rule step <sequences, fails> {
        while let x = Bump(50)? {
            dummy := x
        }
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("argument for parameter"));
}

#[test]
fn call_argument_in_a_deeply_nested_condition_is_checked() {
    // A call buried under an extra operator layer, not just a bare
    // top-level comparison -- confirms `check_calls_in`'s recursive
    // walk (via `sub_exprs`) actually descends, rather than only
    // special-casing a condition that's directly `Call < const`.
    let src = "\
module M {
    reg total : [8] where total < 40 = 0
    Bump(i : [8] where i < 10) : [8] {
        return i
    }
    rule step {
        if (Bump(50) + 1) < 5 {
            total := 1
        } else {
            total := 2
        }
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("argument for parameter"));
}

#[test]
fn call_argument_as_an_argument_to_another_call_is_still_checked() {
    // Resolves the open question left in v12's own scope note -- NOT
    // already true by inspection, as the plan assumed: a call nested as
    // ANOTHER call's own argument (`Outer(Bump(50))`) is only reached
    // via `expr_bound`'s own `Expr::Call` arm recursing into its own
    // args when the OUTER param (`Outer`'s own) has a declared bound to
    // check the arg against -- with no bound there, the old code
    // skipped evaluating that argument's own value entirely (`continue`
    // before ever calling `expr_bound`), silently missing `Bump(50)`'s
    // own violation. Fixed (found empirically while writing this exact
    // test) by calling `expr_bound` on every argument unconditionally;
    // `Outer`'s OWN return bound here just keeps this test isolated to
    // that one fix (otherwise the composition would ALSO be unprovable
    // for its own, unrelated reason, since `Outer` declares none).
    let src = "\
module M {
    reg total : [8] where total < 40 = 0
    Bump(i : [8] where i < 10) : [8] {
        return i
    }
    Outer(x : [8]) : [8] where _ < 40 {
        return 0
    }
    rule step {
        total := Outer(Bump(50))
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("argument for parameter"));
}

#[test]
fn call_argument_in_a_write_to_an_unbounded_reg_is_still_checked() {
    // A second advisor pass found this (and the two tests below) as
    // three MORE instances of the exact "gate the recursive `expr_
    // bound` descent on whether there's a bound to check against"
    // shape the argument-to-another-call fix above already needed --
    // `Stmt::Assign`'s own early returns (non-Ident LHS, unresolvable
    // def, or an UNBOUNDED reg) used to `return` before `expr_bound`
    // ever saw `rhs` at all. `plain` carries no `where` bound, so
    // `plain := Bump(50)` used to skip checking `Bump`'s own argument
    // entirely.
    let src = "\
module M {
    reg plain : [8] = 0
    Bump(i : [8] where i < 10) : [8] {
        return i
    }
    rule step {
        plain := Bump(50)
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("argument for parameter"));
}

#[test]
fn call_argument_in_a_return_with_no_declared_postcondition_is_still_checked() {
    // The `Stmt::Return` mirror of the test above: `current_ret_bound`
    // being `None` (no declared postcondition on the ENCLOSING fn) used
    // to gate the whole `expr_bound` call, so `return Bump(50)` inside
    // a fn with no `where _ < N` never checked `Bump`'s own
    // argument either.
    let src = "\
module M {
    Bump(i : [8] where i < 10) : [8] {
        return i
    }
    Outer(x : [8]) : [8] {
        return Bump(50)
    }
    rule step {
        Outer(3)
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("argument for parameter"));
}

#[test]
fn call_argument_past_an_unprovable_operand_is_still_checked() {
    // The `Add`/`Sub`/`Mul` mirror: `self.expr_bound(*lhs, ...)?`
    // chained directly into `self.expr_bound(*rhs, ...)?` meant an
    // unprovable LHS (`unbounded`, an `in` port with no bound at all)
    // short-circuited the whole arm via `?` BEFORE `rhs` was ever
    // evaluated -- silently skipping `Bump(50)`'s own argument check
    // inside `unbounded + Bump(50)`. Two errors now expected: the
    // argument violation AND the pre-existing "cannot verify this
    // write" (the sum itself was always unprovable, since `unbounded`
    // has no bound -- that part is correct, unchanged behavior).
    let src = "\
module M {
    in unbounded : [8]
    reg total : [8] where total < 40 = 0
    Bump(i : [8] where i < 10) : [8] {
        return i
    }
    rule step {
        total := unbounded + Bump(50)
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 2);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("argument for parameter"))
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("cannot verify this write"))
    );
}

#[test]
fn call_argument_in_a_mem_write_index_is_still_checked() {
    // A fifth instance of the same shape, found by the advisor on a
    // dedicated follow-up probe past the four-site sweep above:
    // `Stmt::Assign`'s LHS handling only ever resolved a bare `Expr::
    // Ident` to a `def`/`bounded` pair -- a mem write's own index
    // expression (`m[Bump(50)] := 1`, an `Expr::Bracket`) was neither
    // that Ident case nor part of `rhs`, so it was never passed to
    // `expr_bound`/`check_calls_in` at all. Fixed by sweeping `lhs`
    // itself through `check_calls_in` unconditionally.
    let src = "\
module M {
    mem m : [8][4]
    Bump(i : [8] where i < 10) : [8] {
        return i
    }
    rule step {
        m[Bump(50)] := 1
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("argument for parameter"));
}

// v15: the `else` branch of an `if` used to inherit the raw, unnarrowed
// entry state -- the negated condition (`i < k`'s `else` is exactly `i
// >= k`) is just as real a proven fact as the condition itself, and it
// was never applied. `Lt`/`Ge`/`Gt` below just mirror an existing
// `narrow_for_condition` formula in the opposite direction; `Ne`'s own
// singleton narrowing is the genuinely NEW capability -- sound for any
// excluded `k`, mid-range included (the same "interval-set domain"
// question left open since v9/v11/v13/v14 turned out to be provably
// inert everywhere it was checked: every check in this file reads only
// extremes, and Add/Sub/Mul are monotonic, so punching an interior hole
// in a THEN-branch range can never move a downstream min/max -- but a
// singleton *else*-branch narrowing needs no interval-set at all, since
// a singleton is just an ordinary one-piece interval).

#[test]
fn else_branch_of_lt_is_narrowed_to_ge() {
    // `else` of `i < 8` is `i >= 8`. Without that fact, `i - 8` is
    // unprovable (i's raw floor is 0, and the subtrahend 8 could exceed
    // it) -- matches examples/else_branch_narrowing.tr, the driving
    // example for this whole feature.
    let src = "\
module M {
    reg total : [8] where total < 3 = 0
    reg i : [8] where i < 10 = 0
    rule step {
        if i < 8 {
            total := 0
        } else {
            total := i - 8
        }
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn else_branch_of_gt_is_narrowed_to_le() {
    // `else` of `i > 5` is `i <= 5`, i.e. `i < 6` -- a NEW formula (this
    // language has no separate `Le` recognized by `narrow_for_condition`
    // itself), not just a mirrored existing one. Without it, `5 - i` is
    // unprovable (i's raw ceiling is 10, exceeding the minuend 5).
    let src = "\
module M {
    reg total : [8] where total < 6 = 0
    reg i : [8] where i < 10 = 0
    rule step {
        if i > 5 {
            total := 0
        } else {
            total := 5 - i
        }
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn else_branch_of_ge_is_narrowed_to_lt() {
    // `else` of `i >= 5` is `i < 5`. Without that fact, `total := i`
    // can't prove `i`'s raw ceiling (10) fits `total`'s declared `< 5`.
    let src = "\
module M {
    reg total : [8] where total < 5 = 0
    reg i : [8] where i < 10 = 0
    rule step {
        if i >= 5 {
            total := 0
        } else {
            total := i
        }
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn else_branch_of_ne_narrows_to_the_exact_singleton_including_mid_range() {
    // The genuinely new capability: `else` of `i <> 5` is the EXACT
    // singleton `i == 5` -- `total`'s declared bound (`5 <= total < 6`,
    // itself the singleton {5}) can only be proven for `total := i` if
    // the else branch narrows `i` down to exactly 5, not just "some
    // value in i's raw [0,10) range". 5 is genuinely mid-range (neither
    // i's floor nor its ceiling), the exact shape v9's own THEN-branch
    // `Ne` narrowing was documented to leave as a no-op.
    let src = "\
module M {
    reg total : [8] where 5 <= total < 6 = 5
    reg i : [8] where i < 10 = 0
    rule step {
        if i <> 5 {
            total := 5
        } else {
            total := i
        }
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn else_branch_of_ne_with_k_outside_the_current_range_stays_unnarrowed() {
    // `i <> 50` when `i`'s declared range is `[0,10)`: 50 is outside
    // i's own proven bound, so the `else` branch (real `i == 50`) is
    // actually unreachable dead code -- narrowing to a bogus singleton
    // `[50,51)` there would fall OUTSIDE i's own declared invariant,
    // risking a wrong VERDICT on the (dead) branch's own writes even
    // though no real unsoundness is possible (a false premise proves
    // anything). `total := i` here only stays provable if the guard
    // (`k` must be within the CURRENT `[lo,hi)`) correctly leaves `i`
    // unnarrowed at its raw `[0,10)` instead, which exactly fits
    // `total`'s own declared `< 10`.
    let src = "\
module M {
    reg total : [8] where total < 10 = 0
    reg i : [8] where i < 10 = 0
    rule step {
        if i <> 50 {
            total := 0
        } else {
            total := i
        }
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn else_branch_of_an_always_true_condition_stays_conservatively_unnarrowed() {
    // An advisor pass caught this: `cnt >= 0` is always true for an
    // unsigned `cnt`, so `else` is unreachable dead code -- but the
    // RAW `Ge`-else formula (`(*lo, k.min(*hi))` with `k = 0`) computes
    // the EMPTY range `(0, 0)`, not just "unnarrowed." Inserting that
    // empty range unclamped let a write with no real relationship to
    // `cnt`'s actual range (`total := cnt`, `total`'s declared bound far
    // narrower than `cnt`'s) slip through as a coincidental VACUOUS
    // accept, rather than the deliberate, documented "leave dead code
    // on the raw entry state" fallback `narrow_for_else`'s own doc
    // comment describes. `total`'s declared bound (`< 3`) is far
    // narrower than `cnt`'s raw declared range (`< 100`), so the
    // correct, conservative behavior here is a REJECTION.
    let src = "\
module M {
    reg total : [8] where total < 3 = 0
    reg cnt : [8] where cnt < 100 = 0
    rule count {
        if cnt >= 0 {
            total := 0
        } else {
            total := cnt
        }
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot verify"));
}

#[test]
fn else_branch_of_a_commuted_ne_also_narrows_to_the_singleton() {
    // The commuted form (`<const> <> <reg>`, v10's own recognized
    // ordering) was a live path through `narrow_for_else`'s own `Ne`-
    // only `or_else` fallback with no dedicated test -- the same
    // "found a real call site with zero coverage" shape v14's own
    // `Stmt::WhileLet` gap was. Same driving shape as the direct-order
    // singleton test above, operands swapped.
    let src = "\
module M {
    reg total : [8] where 5 <= total < 6 = 5
    reg i : [8] where i < 10 = 0
    rule step {
        if 5 <> i {
            total := 5
        } else {
            total := i
        }
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

// `Expr::Bracket` (a mem/fifo access) had no arm in `expr_bound` at all
// through v15 -- it fell to the catch-all `_ => None` with ZERO
// recursion into `callee`/`args`, so a call NESTED inside a mem access
// used as a VALUE (not an assignment target) never had its own argument
// obligations checked. Found empirically while designing a later
// feature (exporting bounds.rs's own per-site facts to schedule.rs),
// not assumed -- confirmed via a driving scratch file showing 0 errors
// on pre-fix code for all three sibling positions below.

#[test]
fn call_argument_in_a_mem_read_used_as_a_value_is_checked() {
    // `examples/mem_read_call_check.tr`'s own driving shape: `y := m
    // [Bump(50)]` is a mem READ, not a write target, so `Stmt::Assign`'s
    // existing LHS-focused fix (the mem-write-index gap) never covered
    // it -- this is the RHS side of the same underlying hole.
    let src = "\
module M {
    mem m : [8][4]
    reg y : [8] = 0
    Bump(i : [8] where i < 10) : [8] {
        return i
    }
    rule step {
        y := m[Bump(50)]
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("argument for parameter"));
}

#[test]
fn call_argument_in_a_mem_read_inside_a_return_is_checked() {
    let src = "\
module M {
    mem m : [8][4]
    Bump(i : [8] where i < 10) : [8] {
        return i
    }
    Get() : [8] {
        return m[Bump(50)]
    }
    rule step {
        Get()
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("argument for parameter"));
}

#[test]
fn call_argument_in_a_mem_read_as_a_call_argument_is_checked() {
    let src = "\
module M {
    mem m : [8][4]
    Bump(i : [8] where i < 10) : [8] {
        return i
    }
    Outer(x : [8]) : [8] where _ < 40 {
        return 0
    }
    reg total : [8] where total < 40 = 0
    rule step {
        total := Outer(m[Bump(50)])
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("argument for parameter"));
}

// v16: `bounds.rs` now exports a per-SITE proven range for a mem
// access's own index expression -- tighter than (or equal to) the
// def's own flat declared range whenever that specific index sits
// under a narrowing condition -- so `schedule.rs`'s own disjointness
// proof can see `if i < 10 { m[i] := x }`'s own narrowed `i < 10` fact,
// not just `i`'s whole-program declared bound. Each test below
// declares the def's range wider than the narrowing (`i < 20` declared,
// `i < 10` narrowed) specifically so the assertion only passes if the
// NARROWED value was exported, not the flat declared one.

#[test]
fn site_range_is_exported_at_a_mem_write_index() {
    let src = "\
module M {
    mem m : [8][20]
    reg i : [8] where i < 20 = 0
    in x : [8]
    rule step {
        if i < 10 {
            m[i] := x
        }
    }
}
";
    let (fx, bounds, errors) = run_with_bounds(src);
    assert!(errors.is_empty(), "{errors:?}");
    let index = the_only_mem_index(&fx);
    assert_eq!(bounds.site_ranges.get(&index), Some(&(0, 10)));
}

#[test]
fn site_range_is_exported_at_a_mem_read_index() {
    let src = "\
module M {
    mem m : [8][20]
    reg i : [8] where i < 20 = 0
    out y : [8] = 0
    rule step {
        if i < 10 {
            y := m[i]
        }
    }
}
";
    let (fx, bounds, errors) = run_with_bounds(src);
    assert!(errors.is_empty(), "{errors:?}");
    let index = the_only_mem_index(&fx);
    assert_eq!(bounds.site_ranges.get(&index), Some(&(0, 10)));
}

#[test]
fn site_range_is_exported_at_a_mem_read_inside_a_return() {
    let src = "\
module M {
    mem m : [8][20]
    reg i : [8] where i < 20 = 0
    Get() : [8] {
        if i < 10 {
            return m[i]
        }
        return 0
    }
    reg y : [8] = 0
    rule step {
        y := Get()
    }
}
";
    let (fx, bounds, errors) = run_with_bounds(src);
    assert!(errors.is_empty(), "{errors:?}");
    let index = the_only_mem_index(&fx);
    assert_eq!(bounds.site_ranges.get(&index), Some(&(0, 10)));
}

#[test]
fn site_range_is_exported_at_a_mem_read_as_a_call_argument() {
    let src = "\
module M {
    mem m : [8][20]
    reg i : [8] where i < 20 = 0
    Identity(v : [8]) : [8] {
        return v
    }
    reg y : [8] = 0
    rule step {
        if i < 10 {
            y := Identity(m[i])
        }
    }
}
";
    let (fx, bounds, errors) = run_with_bounds(src);
    assert!(errors.is_empty(), "{errors:?}");
    let index = the_only_mem_index(&fx);
    assert_eq!(bounds.site_ranges.get(&index), Some(&(0, 10)));
}

#[test]
fn call_argument_hidden_under_a_field_access_in_a_mem_index_is_still_checked() {
    // The exact regression `check_calls_in`'s widened stop-list had to
    // avoid: `Field` is deliberately NOT in the stop-list (unlike `Add`/
    // `Sub`/`Mul`/`Call`/`Bracket`), so a call nested under one is still
    // found via the generic `sub_exprs` recursion -- confirmed via a
    // scratch file before this test was written that `m[MakePair(50)
    // .data]` parses and type-checks as a legal mem index at all (this
    // arc's own standing "measure it, don't assume" discipline).
    let src = "\
module M {
    struct Pair {
        valid : [1]
        data : [8]
    }
    mem m : [8][4]
    MakePair(d : [8] where d < 10) : Pair <combines> {
        return Pair{ valid: 1, data: d }
    }
    reg y : [8] = 0
    rule step {
        y := m[MakePair(50).data]
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("argument for parameter"));
}

#[test]
fn mem_elem_write_within_bound_is_proven() {
    let src = "\
module M {
    mem m : [8][10] where _ < 50
    reg i : [8] where i < 10 = 0
    rule write {
        if i < 10 {
            m[i] := 40
        }
    }
}
";
    let errors = run(src);
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn mem_elem_write_exceeding_bound_is_rejected() {
    let src = "\
module M {
    mem m : [8][10] where _ < 50
    reg i : [8] where i < 10 = 0
    rule write {
        if i < 10 {
            m[i] := 60
        }
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("computed value could reach 60"));
}

#[test]
fn struct_field_write_within_bound_is_proven() {
    let src = "\
module M {
    struct Pair {
        valid : [1]
        data : [8] where _ < 50
    }
    reg p : Pair = Pair{ valid: 0, data: 0 }
    rule write {
        p := Pair{ valid: 1, data: 40 }
    }
}
";
    let errors = run(src);
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn struct_field_write_exceeding_bound_is_rejected() {
    let src = "\
module M {
    struct Pair {
        valid : [1]
        data : [8] where _ < 50
    }
    reg p : Pair = Pair{ valid: 0, data: 0 }
    rule write {
        p := Pair{ valid: 1, data: 60 }
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("computed value could reach 60"));
}

#[test]
fn struct_field_read_value_composes_through_a_reg() {
    // v18's own actual new capability, unlike v17's mem-element bound
    // (which shipped WITHOUT read composition, see `mem_elem_read_
    // value_is_deliberately_not_composed` above): a struct-typed reg's
    // own field value DOES soundly compose at a read, since every
    // possible value it could ever hold passed through a checked
    // `StructLit` -- its mandatory `= init` or a later whole-value
    // write, never an "unwritten address" the way a mem read can be.
    let src = "\
module M {
    struct Pair {
        valid : [1]
        data : [8] where _ < 50
    }
    reg p : Pair = Pair{ valid: 0, data: 0 }
    reg total : [8] where total < 50 = 0
    rule write {
        p := Pair{ valid: 1, data: 40 }
    }
    rule read {
        total := p.data
    }
}
";
    let errors = run(src);
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn struct_field_read_value_from_an_in_port_is_deliberately_not_composed() {
    // The critical negative test, mirroring `mem_elem_read_value_is_
    // deliberately_not_composed`'s own role for v17: a struct-typed
    // `in` port's value never passes through a checked `StructLit` at
    // all (it arrives over an external wire), exactly as untrusted as
    // an unwritten mem address -- confirmed real and legal by `tests/
    // firrtl.rs`'s own `struct_typed_input_port_flattens_and_reads_by_
    // field`. Bug-reintroduction-verified during development by
    // temporarily widening `struct_field_bound`'s `DefKind` match to
    // also accept `Input`, which flips this test from 1 error to 0.
    let src = "\
module M {
    struct Pair {
        valid : [1]
        data : [8] where _ < 50
    }
    in q : Pair
    reg total : [8] where total < 50 = 0
    rule read {
        total := q.data
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("cannot verify this write stays within the declared bound")
    );
}

#[test]
fn struct_field_read_composes_through_a_dotdot_base_spread() {
    // A `..base`-filled field (the field omitted from an explicit
    // `StructLit`, backfilled from `base`'s own same-named field) must
    // still compose through `struct_field_bound`'s own recursive
    // `base?` fallback, not silently fail to `None`.
    let src = "\
module M {
    struct Pair {
        valid : [1]
        data : [8] where _ < 50
    }
    reg p : Pair = Pair{ valid: 0, data: 10 }
    reg total : [8] where total < 50 = 0
    rule write {
        p := Pair{ valid: 1, ..p }
    }
    rule read {
        total := p.data
    }
}
";
    let errors = run(src);
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn struct_field_read_through_an_aliased_local_is_deliberately_not_composed() {
    // The one case that requires `struct_field_bound`'s recursive
    // `struct_origins` trace to work correctly, not just the direct
    // cases: `let p2 = q` binds a struct-typed LOCAL to an ALIAS of an
    // untrusted `in`-port value (not a fresh `StructLit`) -- `p2.data`
    // must still fail to compose, by recursing through `p2`'s own
    // recorded origin (`q`) and hitting the same `DefKind` gate a
    // direct `q.data` read already does.
    let src = "\
module M {
    struct Pair {
        valid : [1]
        data : [8] where _ < 50
    }
    in q : Pair
    reg total : [8] where total < 50 = 0
    rule read {
        let p2 = q
        total := p2.data
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("cannot verify this write stays within the declared bound")
    );
}

#[test]
fn struct_field_write_from_a_call_is_rejected() {
    // A documented v1 restriction, not a silent skip: a struct-typed
    // write whose RHS is a `Call` (the type-checker's OTHER permitted
    // shape alongside a plain `StructLit`, `types/stmt.rs:450`) has no
    // field-by-field verification mechanism here at all, so it must
    // conservatively fail rather than pass unchecked.
    let src = "\
module M {
    struct Pair {
        valid : [1]
        data : [8] where _ < 50
    }
    MakePair() : Pair <combines> {
        return Pair{ valid: 1, data: 10 }
    }
    reg p : Pair = Pair{ valid: 0, data: 0 }
    rule write {
        p := MakePair()
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("cannot verify this write to field")
    );
}

#[test]
fn struct_field_write_exceeding_bound_is_rejected_with_no_other_bounded_def() {
    // Discriminates `check_item`'s own FOUR-way early-return guard: this
    // module has NO other bounded reg/out/param/return/mem anywhere,
    // only the bounded struct field -- if the guard didn't include
    // `struct_field_bounds`, `check_item` would bail out before ever
    // walking this rule's body, and the out-of-range write below would
    // be silently accepted instead of rejected. Caught by advisor
    // review of this feature's own plan before any code was written.
    let src = "\
module M {
    struct Pair {
        valid : [1]
        data : [8] where _ < 50
    }
    reg p : Pair = Pair{ valid: 0, data: 0 }
    rule write {
        p := Pair{ valid: 1, data: 60 }
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("computed value could reach 60"));
}

#[test]
fn struct_field_bad_init_is_rejected() {
    // The base case: a struct-typed reg/out's own `= init` is checked
    // against its bounded fields too, entirely within `bounds.rs`
    // (`check_struct_field_inits`) rather than `types/stmt.rs` -- see
    // that function's own doc comment for why this feature doesn't need
    // `types.rs`'s `const_eval` the way a scalar bound's own base case
    // does.
    let src = "\
module M {
    struct Pair {
        valid : [1]
        data : [8] where _ < 50
    }
    reg p : Pair = Pair{ valid: 0, data: 60 }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot verify this init field"));
}

#[test]
fn struct_field_missing_init_is_rejected() {
    // A real soundness hole found by an advisor pass AFTER this feature
    // had already shipped, not during its own design/implementation:
    // unlike a scalar `where`-bounded reg/out (whose init requirement is
    // enforced by `parse_state_decl`, triggered by that def's OWN
    // `where` clause), NOTHING forced an init here -- the bound lives on
    // the STRUCT FIELD, not on `p` itself, so `reg p : Pair` with no
    // init at all was ordinary, legal syntax the parser has no way to
    // reject (it doesn't know at parse time whether `Pair` has any
    // bounded field). Without `check_struct_field_inits` rejecting a
    // missing init, `p.data` would still be unconditionally trusted at
    // every read -- concretely a FALSE proof for this two-sided bound,
    // since a struct-typed reg with no init resets to all-zero fields in
    // FIRRTL, which violates a floor of 10. Confirmed via direct
    // reproduction before this test was written: this exact source
    // compiled with ZERO errors prior to the fix.
    let src = "\
module M {
    struct Pair {
        data : [8] where 10 <= _ < 20
    }
    reg p : Pair
    reg total : [8] where 10 <= total < 20 = 10
    rule r {
        total := p.data
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("needs an explicit `= init`"));
}

#[test]
fn struct_field_read_through_a_reassigned_local_is_deliberately_not_composed() {
    // A second real soundness hole found by the same advisor pass: a
    // struct-typed LOCAL can be REASSIGNED via `:=` (unlike a reg/out,
    // `types/stmt.rs` places no "must be a StructLit or Call" shape
    // restriction on a `DefKind::Local` target), so `struct_origins`'s
    // entry from this local's own `Stmt::Let` would otherwise keep
    // pointing at its ORIGINAL binding forever: `p.data` traced back to
    // the stale, already-superseded `Pair{data: 15}` literal instead of
    // `q` (an untrusted `in`-port value) that `p` was actually
    // reassigned to. Confirmed via direct reproduction before this test
    // was written: this exact source compiled with ZERO errors prior to
    // the fix (`Stmt::Assign` now overwrites `struct_origins` with the
    // new rhs on every reassignment to a struct-typed local, exactly
    // like `Stmt::Let` already does for the initial binding).
    let src = "\
module M {
    struct Pair {
        data : [8] where 10 <= _ < 20
    }
    in q : Pair
    reg total : [8] where 10 <= total < 20 = 10
    rule r {
        let p = Pair{ data: 15 }
        p := q
        total := p.data
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("cannot verify this write stays within the declared bound")
    );
}

#[test]
fn struct_field_two_sided_bound_composes_through_a_reg() {
    // Every OTHER struct-field test in this file uses a one-sided bound
    // (`where _ < 50`), where a default-0 reset value happens to satisfy
    // the bound regardless of whether the base case is actually
    // checked -- the two-sided form (`where 10 <= _ < 20`) is the only
    // shape that turns a missed base-case check into an OBSERVABLE false
    // proof (flagged by advisor review as a real gap in this suite's own
    // coverage, independent of the two bugs above). This pins the
    // ordinary positive case with a two-sided bound specifically, so a
    // future regression in the base-case check can't hide behind an
    // always-satisfied floor of 0.
    let src = "\
module M {
    struct Pair {
        data : [8] where 10 <= _ < 20
    }
    reg p : Pair = Pair{ data: 15 }
    reg total : [8] where 10 <= total < 20 = 10
    rule write {
        p := Pair{ data: 12 }
    }
    rule read {
        total := p.data
    }
}
";
    let errors = run(src);
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn reassigned_scalar_local_read_is_deliberately_not_composed() {
    // A THIRD real soundness hole, found by yet another advisor pass
    // (called after the two struct-field fixes above had already
    // shipped): `locals` -- the forward-flow map behind EVERY local's
    // own provable bound, not just a struct field's -- is written ONLY
    // at `Stmt::Let` and nowhere else, a gap present since the very
    // first bounds.rs commit, predating v18 entirely. `x`'s stale
    // `let`-time bound (`10`) kept composing at `total := x` even with
    // a DIRECT, unbranched reassignment to an untrusted `in`-port value
    // in between -- confirmed via direct reproduction: this exact
    // source compiled with ZERO errors before the fix (a pre-scan,
    // `collect_reassigned_locals`, that finds every `DefKind::Local`
    // ever reassigned anywhere in a body and refuses to give it a
    // trusted `locals`/`struct_origins` entry at all).
    let src = "\
module M {
    in q : [8]
    out total : [8] where total < 20 = 0
    rule r {
        let x = 10
        x := q
        total := x
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("cannot verify this write stays within the declared bound")
    );
}

#[test]
fn reassigned_scalar_local_in_a_branch_is_deliberately_not_composed() {
    // The same hole, reached through a branch: `locals`/`struct_
    // origins` clone-and-discard per nested scope (so a bound LEARNED
    // inside an `if` doesn't leak out), which made a first, narrower
    // fix attempt (poison an outer entry on branch exit if the clone's
    // value changed) look plausible -- but the pre-scan approach below
    // makes this case unremarkable: `x` never gets an entry in the
    // first place, in ANY scope, so there's nothing branch-local to
    // leak or fail to poison.
    let src = "\
module M {
    in cond : [1]
    in q : [8]
    out total : [8] where total < 20 = 0
    rule r {
        let x = 10
        if cond = 1 {
            x := q
        }
        total := x
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("cannot verify this write stays within the declared bound")
    );
}

#[test]
fn reassigned_scalar_local_in_a_while_loop_is_deliberately_not_composed() {
    // The case that rules out branch-exit poisoning as a fix entirely:
    // a `while` loop's body is checked ONCE against its entry snapshot
    // (DESIGN.md's `<sequences>` lowering -- each iteration is its own
    // clock edge, but this pass doesn't unroll or fixed-point over
    // iterations), so `total := x` here would be checked against
    // iteration 1's `x` even though iteration 2+ actually has `x = q`.
    // Confirmed via direct reproduction: this exact source compiled
    // with ZERO errors before the pre-scan fix, with NO merge/poison
    // logic able to fix it (there is no branch exit to poison at all
    // here -- `x := q` and `total := x` are both inside the SAME loop
    // body, checked in the same single pass).
    let src = "\
module M {
    in q : [8]
    reg i : [4] where i < 9 = 0
    out total : [8] where total < 20 = 0
    rule bump <sequences> {
        let x = 10
        while i < 3 {
            total := x
            x := q
            i := i + 1
            tick
        }
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("cannot verify this write stays within the declared bound")
    );
}

#[test]
fn mem_elem_read_value_is_deliberately_not_composed() {
    // A soundness-restriction regression guard, not a capability test: an
    // earlier version of this feature handed the mem's declared bound
    // back at a READ site too, composing `total := m[i]` cleanly. An
    // advisor pass caught a real hole in that before it shipped -- a mem
    // has no `init`/reset, so proving every WRITE stays in range says
    // nothing about what an unwritten or not-yet-written READ returns,
    // and that unproven value could otherwise reach `schedule.rs`'s own
    // disjointness proof via `let a = m[pc]; m[a]`. So a mem read's own
    // value must still compose to `None`, exactly as before this
    // feature existed -- this test pins that restriction so a future
    // change can't silently reintroduce the hole.
    let src = "\
module M {
    mem m : [8][10] where _ < 50
    reg i : [8] where i < 10 = 0
    reg total : [8] where total < 50 = 0
    rule write {
        if i < 10 {
            m[i] := 40
        }
    }
    rule read {
        total := m[i]
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("cannot verify this write stays within the declared bound")
    );
}

#[test]
fn mem_bound_never_written_is_rejected() {
    // v17's own sibling of v13's `check_return_site_exhaustiveness`,
    // narrower in reach than v13's own version (see `check_mem_bound_is_
    // proven`'s own doc comment: a mem read no longer trusts this bound
    // at all, so this isn't closing a read-trust hole -- it's flagging
    // dead, misleading metadata: a declared bound whose only real
    // obligation, the write-site check, is never exercised). `y` is
    // plain (no `where`), so this test isolates that one error, not a
    // second, unrelated "cannot verify this write" from `y`'s own body.
    let src = "\
module M {
    mem m : [8][10] where _ < 50
    out y : [8] = 0
    rule read {
        y := m[0]
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("this mem bound is never checked against an actual write")
    );
}

#[test]
fn mem_elem_write_exceeding_bound_is_rejected_with_no_other_bounded_def() {
    // Discriminates `check_item`'s own three-way early-return guard: this
    // module has NO bounded reg/out/param/return anywhere, only the
    // bounded mem -- if the guard were still the old two-way check (v16
    // and earlier), `check_item` would bail out before ever walking this
    // rule's body, and the out-of-range write below would be silently
    // accepted instead of rejected.
    let src = "\
module M {
    mem m : [8][10] where _ < 50
    in i : [8]
    rule write {
        m[i] := 60
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("computed value could reach 60"));
}
