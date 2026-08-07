use trace::ast::{Ast, Item, ItemId};
use trace::bounds::{self, Bounds, BoundsError};
use trace::effects::Effects;
use trace::resolve::{DefId, Resolution};
use trace::types::Types;
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

/// Full pipeline including `Ast`/`Resolution`/`Types` -- `provably_
/// disjoint_mem_indices` needs all three (rule bodies for guards, `Types`
/// for its own leaf-width soundness check) alongside `Bounds` itself.
fn run_full(src: &str) -> (Ast, Resolution, Types, Bounds, Vec<BoundsError>) {
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
    (ast, res, ty, bounds, errors)
}

/// The sole mem-index `ExprId` reached in `rule`'s own body, paired with
/// the guards active at that exact site -- `bounds::guarded_mem_
/// accesses`'s own per-rule walk, restricted to one rule instead of
/// `the_only_mem_index`'s whole-program aggregation (needed here since a
/// test's two rules each have their own single mem access, and the two
/// must not be conflated).
fn the_only_mem_index_in_rule(
    ast: &Ast,
    res: &Resolution,
    rule: ItemId,
) -> (
    trace::ast::ExprId,
    Vec<trace::ast::ExprId>,
    Vec<trace::ast::ExprId>,
) {
    let Item::Rule { body, .. } = ast.item(rule) else {
        panic!("expected a rule");
    };
    let accesses = bounds::guarded_mem_accesses(ast, res, body);
    assert_eq!(
        accesses.len(),
        1,
        "expected exactly one mem index site in rule"
    );
    let (idx, (guards, guards_negated)) = accesses.into_iter().next().unwrap();
    (idx, guards, guards_negated)
}

/// Any `Expr::Ident` in the whole program resolving to `def` -- used to
/// feed `provably_disjoint_mem_indices` a bare register reference
/// directly (not a real mem-index site) when a test wants to check the
/// relational-fact argument alone, independent of any actual `m[...]`
/// access.
fn any_ident_for(ast: &Ast, res: &Resolution, def: DefId) -> trace::ast::ExprId {
    for i in 0..ast.exprs.len() {
        let id = trace::ast::ExprId(i as u32);
        if matches!(ast.expr(id), trace::ast::Expr::Ident(_))
            && res.expr_defs.get(&id) == Some(&def)
        {
            return id;
        }
    }
    panic!("no Ident expression resolves to {def:?}");
}

/// The one `DefId` whose own name matches `name` exactly -- panics on
/// zero or multiple matches, same "keep the driving shape unambiguous"
/// convention `the_only_mem_index` (above) already follows.
fn def_named(res: &Resolution, name: &str) -> DefId {
    let matches: Vec<DefId> = res
        .defs
        .iter()
        .enumerate()
        .filter(|(_, d)| d.name == name)
        .map(|(i, _)| DefId(i as u32))
        .collect();
    assert_eq!(matches.len(), 1, "expected exactly one def named `{name}`");
    matches[0]
}

/// The one `Item::Rule` whose own name matches `name` exactly.
fn rule_named(ast: &Ast, name: &str) -> ItemId {
    fn walk(ast: &Ast, items: &[ItemId], name: &str, found: &mut Vec<ItemId>) {
        for &id in items {
            match ast.item(id) {
                Item::Module { items, .. } => walk(ast, items, name, found),
                Item::Rule { name: n, .. } if n.text == name => found.push(id),
                _ => {}
            }
        }
    }
    let mut found = Vec::new();
    walk(ast, &ast.roots, name, &mut found);
    assert_eq!(found.len(), 1, "expected exactly one rule named `{name}`");
    found[0]
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
            i := (i + 1).!
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
        i := (i + 1).!
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot verify"));
}

#[test]
fn where_bound_limit_composed_from_literal_arithmetic_is_still_enforced() {
    // A regression pin: `bounds.rs`'s own `const_fold` used to handle
    // only a bare literal, unlike `types.rs`'s `const_eval` (which
    // folds `Add`/`Sub`/etc of literals too) -- so `where cnt < 8 + 2`
    // used to type-check clean while the bound silently never made it
    // into `self.bounded` at all, leaving `cnt`'s write completely
    // unchecked with zero diagnostic. `8 + 2` here must behave exactly
    // like the equivalent literal `10` does in `unguarded_increment_
    // is_rejected` above -- same rejection, not a silent pass.
    let src = "\
module M {
    reg cnt : [8] where cnt < 8 + 2 = 0
    rule bump {
        cnt := (cnt + 100).!
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("declared bound `0 <= _ < 10`"));
}

#[test]
fn wildcard_self_reference_on_a_reg_bound_is_enforced_identically_to_the_name_form() {
    // Stage 3's own `_`-placeholder unification: `resolve.rs`'s `check_
    // bound_self_reference` now accepts `_` for a reg/out/param bound,
    // not just the literal name -- `bounds.rs`'s own collector never
    // reads the LHS once resolve.rs approves it, so this must reject
    // the exact same over-limit write `unguarded_increment_is_rejected`
    // (above) does, byte-for-byte, just spelled with `_` instead of `i`.
    let src = "\
module M {
    reg i : [4] where _ < 9 = 0
    rule bump {
        i := (i + 1).!
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot verify"));
}

// Stage 3's own operator generalization (v21): `parse_where_bound` used
// to hardcode the top-level relation to `Lt`; it now accepts `<`/`<=`/
// `>`/`>=`, normalized by `ast::normalize_where_relation` (shared with
// `types/stmt.rs`'s own init-value check) into the same `[lower, upper)`
// interval shape the engine already understood. The following pin each
// operator, plus commutation (self on either side) and the width-
// boundary case the normalization itself is riskiest at.

#[test]
fn where_bound_ge_operator_is_recognized() {
    let src = "\
module M {
    reg cnt : [8] where cnt >= 5 = 5
    rule step {
        cnt := 3
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("declared bound `5 <= _ < 256`"));
    assert!(errors[0].message.contains("could go below 5"));
}

#[test]
fn where_bound_gt_operator_is_recognized() {
    let src = "\
module M {
    reg cnt : [8] where cnt > 5 = 6
    rule step {
        cnt := 5
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("declared bound `6 <= _ < 256`"));
}

#[test]
fn where_bound_le_operator_is_recognized() {
    let src = "\
module M {
    reg cnt : [8] where cnt <= 5 = 0
    rule step {
        cnt := (cnt + 6).!
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("declared bound `0 <= _ < 6`"));
}

#[test]
fn where_bound_le_operator_accepts_a_write_at_the_inclusive_limit() {
    // The other side of `where_bound_le_operator_is_recognized`: `<= 5`
    // normalizes to `[0, 6)`, so writing exactly 5 (the inclusive limit
    // itself) must be accepted, not rejected off-by-one.
    let src = "\
module M {
    reg cnt : [8] where cnt <= 5 = 0
    rule step {
        cnt := 5
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn commuted_where_bound_operator_is_recognized() {
    // `parse_where_bound` parses positionally and doesn't track which
    // operand the user meant as self -- `10 > cnt` must be recognized
    // exactly as `cnt < 10` is, self on the RHS this time.
    let src = "\
module M {
    reg cnt : [8] where 10 > cnt = 0
    rule step {
        cnt := (cnt + 20).!
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("declared bound `0 <= _ < 10`"));
}

#[test]
fn where_bound_ge_zero_at_full_width_still_catches_width_overflow() {
    // The riskiest boundary case in `normalize_where_relation`: `_ >=
    // 0` on a `[4]`-wide reg normalizes to `[0, 16)` -- `upper == 2^
    // width` EXACTLY, not greater than it, so this must NOT spuriously
    // trip `check_against_bound`'s separate width-overflow check (`hi >
    // 2^width`); it must behave byte-identically to the equivalent `_ <
    // 16` form, which correctly rejects an unguarded `cnt + 1` near the
    // storage boundary (verified by hand against the `< 16` form before
    // writing this test: same error, same message).
    let src = "\
module M {
    reg cnt : [4] where cnt >= 0 = 0
    rule step {
        cnt := (cnt + 1).!
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("could reach or exceed the declared width")
    );
}

#[test]
fn where_bound_ge_operator_is_recognized_on_a_param_bound() {
    // Confirms the operator generalization applies uniformly to all
    // four `collect_one_*` collectors, not just the scalar reg/out
    // case above -- this one exercises `collect_one_bounded_def`'s own
    // PARAM path (v12's own attachment point). `Bump`'s own body is
    // deliberately trivial (no arithmetic on `i`): `i`'s own declared
    // bound (`>= 5`, no upper) is intentionally wide open on top, so
    // composing `i` further here would trip the SEPARATE width-overflow
    // check for an unrelated reason -- this test isolates the one
    // thing it's actually pinning, the call-site argument check.
    let src = "\
module M {
    reg y : [8] where y < 3 = 0
    reg cnt : [8] = 0
    Bump(i : [8] where i >= 5) {
        cnt := 0
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
            i := (i + 1).!
        }
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot verify"));
}

// A real user report, not anticipated: `narrow_for_condition`/`narrow_
// for_else` recognized `Lt`/`Gt`/`Ge`/`Ne` as bare guard-narrowing
// operators, but never `Le` -- unlike `where`-bound declarations, which
// gained `<=` support at stage 3 (`normalize_where_relation`). A `<=`
// guard silently narrowed NOTHING (fell through the `match op`'s own
// `_ => {}` catch-all), so `b <= 0b1111` left `b` at its full declared
// range instead of narrowing its upper bound -- reproduces identically
// on the pre-fix binary, confirming this predates and is independent of
// this session's earlier inlining work. `translate_guard` (smt.rs) had
// the identical gap, its own doc comment stating it must stay a
// "faithful SHADOW" of `narrow_for_condition` -- fixed in the same
// commit to avoid the two silently drifting apart.

#[test]
fn le_guard_narrows_a_subsequent_write() {
    let src = "\
module M {
    reg i : [4] where i < 10 = 0
    rule bump {
        if i <= 8 {
            i := (i + 1).!
        }
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn insufficiently_narrowed_le_guard_is_rejected() {
    // `if i <= 8` composed with `i`'s own declared `< 9` doesn't narrow
    // beyond the declared bound at all (`i <= 8` IS `i < 9`) -- the
    // composed bound (`8 + 1 + 1 = 10`, i.e. `i` could reach 9) still
    // exceeds `< 9`. Mirrors `insufficiently_narrowed_guard_is_rejected`
    // above, spelled with `<=` instead of `<`.
    let src = "\
module M {
    reg i : [4] where i < 9 = 0
    rule bump {
        if i <= 8 {
            i := (i + 1).!
        }
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot verify"));
}

#[test]
fn bare_le_comparison_narrows_subsequent_writes() {
    // The exact user-reported shape: a BARE `<=` comparison statement
    // (no `if`, implicitly gating the whole rule) must narrow `b`'s
    // upper bound just like the bare `<` form already does.
    let src = "\
module M {
    in a : [1]
    out b : [5] where _ > 1 = 5
    rule step {
        a?
        b <= 0b1110
        b := (b + 1).!
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

// A real user report, not anticipated: DESIGN.md's own "Comparisons:
// fallible by default" section says a bare comparison statement (no
// `if`, no explicit `?`) implicitly gates the WHOLE enclosing rule --
// exactly like `(cond)?` -- and every other pass in this compiler
// (`effects.rs`'s `sig.fails`, `firrtl/writes.rs`'s `compile_guard`)
// already implements this. `bounds.rs`'s own per-statement walk
// (`check_stmt`'s `Stmt::Expr` arm) never did: a write AFTER a bare
// guard was checked against the UNNARROWED declared bound, rejecting
// programs firtool would happily accept. Fixing `check_stmt` alone
// would have left `shadow_walk_body` (the SMT shadow check's own
// independent reconstruction) stale -- it compares its OWN
// reconstruction against Z3, not against `check_stmt`'s real live
// state, so both silently agreeing (both still unnarrowed) would have
// masked the real engine's divergence with no panic at all. Both are
// fixed together here.

#[test]
fn bare_comparison_statement_narrows_subsequent_writes() {
    let src = "\
module M {
    in a : [1]
    out b : [5] where _ > 1 = 5
    rule step {
        a?
        b <> 0b11111
        b := (b + 1).!
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn explicit_guard_sugar_narrows_subsequent_writes() {
    // Same shape as `bare_comparison_statement_narrows_subsequent_
    // writes`, spelled with the explicit `(cond)?` form instead of the
    // bare comparison -- both must narrow identically (`check_stmt`'s
    // new case handles `Expr::Guard(inner)` and a bare comparison via
    // the same `is_guard_like` gate).
    let src = "\
module M {
    in a : [1]
    out b : [5] where _ > 1 = 5
    rule step {
        a?
        (b <> 0b11111)?
        b := (b + 1).!
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn bare_comparison_guard_does_not_narrow_a_write_before_it() {
    // A deliberate v1 scope cut, not a soundness gap: this fix only
    // narrows FORWARD in program order. `firrtl/writes.rs`'s own
    // `compile_guard` doesn't require program order at all (the rule's
    // fire signal is one AND of every guard-like condition anywhere in
    // the body, regardless of position) -- reproducing that here would
    // need a two-pass walk, left for later. A write textually BEFORE
    // the guard is still checked against the unnarrowed bound, so this
    // program is still (conservatively, correctly) rejected.
    let src = "\
module M {
    in a : [1]
    out b : [5] where _ > 1 = 5
    rule step {
        a?
        b := (b + 1).!
        b <> 0b11111
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("could reach or exceed the declared width")
    );
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
            cnt := (cnt + 1).!
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
            cnt := (cnt + 50).!
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
            i := (i + 1).!
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
        i := (i * 1).!
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
            cnt := (cnt * 2).!
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
        cnt := (cnt * 2).!
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
            i := (i * 3).!
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
            cnt := (cnt * 2).!
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
            cnt := (cnt * 2).!
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
        i := (i + 1).!
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
            i := (next).!
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
            i := (step).!
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
            i := (step).!
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
        i := (i + 1).!
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
            i := (i + 1 + 1).!
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
            j := (j + 1).!
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
            i := (i + 1).!
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
        i := (i + 1).!
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
        cnt := (i + 1).!
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
        cnt := (i + 1).!
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
        cnt := (i + 1).!
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
        return (i + 5).!
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
        return (i + 100).!
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
        return (i + 5).!
    }
    rule step {
        total := (Bump(3) + Bump(4)).!
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
        return (i + 5).!
    }
    rule step {
        total := (Bump(3) + Bump(4)).!
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
        total := (Bump(3) + Bump(4)).!
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
fn call_with_no_postcondition_composes_via_body_substitution_inlining() {
    // A fn with NO declared postcondition used to compose to `None`
    // unconditionally (opt-in propagation only). It now falls back to
    // body-substitution inlining: `Bump`'s single-statement `return i +
    // 5` body is evaluated directly with `i` bound to each call's own
    // argument range, so `Bump(3) + Bump(4)` composes to `(3+5) + (4+5)
    // = [17, 17]`, well within `total`'s own declared bound.
    let src = "\
module M {
    reg total : [8] where total < 40 = 0
    Bump(i : [8] where i < 10) : [8] {
        return (i + 5).!
    }
    rule step {
        total := (Bump(3) + Bump(4)).!
    }
}
";
    let errors = run(src);
    assert!(errors.is_empty(), "errors: {errors:?}");
}

#[test]
fn multi_statement_body_with_no_postcondition_is_still_unprovable() {
    // Body-substitution inlining is restricted to a SINGLE `return`
    // statement (v1 restriction: a multi-statement body could contain
    // its own `Stmt::Let`s this walk doesn't track). A fn with no
    // declared postcondition and a body inlining can't handle still
    // composes to `None`, same as before this feature existed.
    let src = "\
module M {
    reg total : [8] where total < 40 = 0
    Bump(i : [8] where i < 10) : [8] {
        let extra = 5
        return (i + extra).!
    }
    rule step {
        total := (Bump(3) + Bump(4)).!
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot verify this write"));
}

#[test]
fn generic_call_with_shift_and_trunc_composes_via_inlining() {
    // Real user-reported false negative: `b`'s own inductive bound
    // (`_ > 1`, narrowed to `[2, 15)` under the guard `b < 0b1111`)
    // proves `Double(trunc(b, 4))` stays in range (`[4, 29)`), but
    // needed THREE capabilities landing together to actually compose:
    // `trunc`'s own identity-when-it-fits shape, `Double`'s `x << 1`
    // body composing via `Shl`, and body-substitution inlining letting
    // `Double`'s own declared-postcondition-free return propagate at
    // all.
    let src = "\
module M {
    in a : [1]
    out b : [5] where _ > 1 = 5

    Double(x : [n]) : [n + 1] { return x << 1 }

    rule step {
        a?
        b < 0b1111
        b := Double(trunc(b, 4))
    }
}
";
    let errors = run(src);
    assert!(errors.is_empty(), "errors: {errors:?}");
}

#[test]
fn generic_call_with_shift_and_trunc_composes_under_a_le_guard() {
    // A second real user report on the same program: swapping `b <
    // 0b1111` for `b <= 0b1111` (the user's own exact edit) should be
    // an equivalent-in-spirit guard (both narrow `b` to `[2, 16)`) and
    // equally provable -- but exposed a SEPARATE, pre-existing,
    // independent gap: `narrow_for_condition` never recognized `Le` as
    // a narrowing operator at all, so this guard narrowed nothing and
    // `b` stayed at its full declared `[2, 32)`, which doesn't fit
    // `trunc`'s identity tier (`hi <= cap` fails at 32 > 16), falling to
    // the coarser `[0, 16)` fallback and failing the write's own LOWER
    // bound (`0 < 2`). Confirmed via the pre-fix binary that this
    // reproduces identically without ANY of this session's earlier
    // inlining/Shl/trunc work -- a real, independent bug, not a
    // regression from that work.
    let src = "\
module M {
    in a : [1]
    out b : [5] where _ > 1 = 5

    Double(x : [n]) : [n + 1] { return x << 1 }

    rule step {
        a?
        b <= 0b1111
        b := Double(trunc(b, 4))
    }
}
";
    let errors = run(src);
    assert!(errors.is_empty(), "errors: {errors:?}");
}

#[test]
fn implicit_trunc_width_inferred_from_write_target_composes() {
    // A third variant of the same driving example: `trunc(b, 4)` (an
    // explicit width) replaced with `trunc(b)` (1-arg, implicit) --
    // `type_call`'s hint-based backward-fill (v24, `types/expr.rs`)
    // solves `Double`'s own `n` from `b`'s declared write-target width
    // (`[5]`) against `Double`'s `[n + 1]` return shape (`n = 4`),
    // purely type-level, with zero value-range reasoning of its own --
    // then this pass's `trunc` arm still independently re-proves that
    // width is lossless under `b`'s own guard-narrowed range, exactly
    // like the explicit-width case above.
    let src = "\
module M {
    in a : [1]
    out b : [5] where _ > 1 = 5

    Double(x : [n]) : [n + 1] { return x << 1 }

    rule step {
        a?
        b < 0b1111
        b := Double(trunc(b))
    }
}
";
    let errors = run(src);
    assert!(errors.is_empty(), "errors: {errors:?}");
}

#[test]
fn implicit_trunc_width_inference_rejects_unproven_truncation() {
    // Advisor-caught during review of the feature above: the hint-based
    // width solve is PURELY type-level (matching the callee's return
    // shape against the caller's write-target width) and has no
    // connection to whether the value being truncated actually fits.
    // `Double(x : [n]) : [n + 2]` into an UNGUARDED `b : [5]` solves
    // `n = 3` (the only width making `n + 2` equal `5`), and without
    // this check `trunc(b)` would silently mask away `b`'s top two bits
    // with zero diagnostic -- a real silent-narrowing bug caught before
    // ever landing, not a hypothetical. Confirmed via bug-reintroduction
    // (see the `trunc` arm's own doc comment in `bounds/mod.rs`): this
    // test fails to produce an error at all if the `is_implicit` check
    // is removed.
    let src = "\
module M {
    in a : [1]
    out b : [5] = 5

    Double(x : [n]) : [n + 2] { return x << 1 }

    rule step {
        a?
        b := Double(trunc(b))
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1, "errors: {errors:?}");
    assert!(errors[0].message.contains("discards no bits"));
}

#[test]
fn implicit_trunc_as_a_bare_write_rhs_is_still_unresolvable() {
    // No regression to documented pre-v24 behavior: a 1-arg `trunc`
    // used directly as a write's RHS (not as an argument to a generic
    // call) has no caller-side hint to solve a width from at all --
    // `type_call`'s backward-fill only ever fires for a trunc call
    // that's itself an ARGUMENT, so this still fails exactly like it
    // did before this feature existed.
    let src = "\
module M {
    in a : [1]
    out b : [5] where _ > 1 = 5
    rule step {
        a?
        b < 0b1111
        b := trunc(b)
    }
}
";
    let errors = run(src);
    assert!(!errors.is_empty(), "expected an error, got none");
}

#[test]
fn lossy_trunc_bypasses_the_implicit_width_losslessness_proof() {
    // v25: `trunc.!(value)` -- the same per-application "I know, let it
    // through" `.!` suffix `a >>.! 300` already has on a binary
    // operator (`ast.lossy`), extended to a call. This is the EXACT
    // `Double(x:[n]):[n+2]` shape that `implicit_trunc_width_inference_
    // rejects_unproven_truncation` (above) correctly rejects for plain
    // `trunc(b)` -- `trunc.!(b)` opts INTO the old, pre-v24 silent
    // masking behavior instead, same treatment the explicit 2-arg form
    // has always gotten (the fallback `[0, cap)` range, not an error).
    //
    // `y`'s own unrelated `where` bound exists ONLY to keep `check_
    // item`'s five-way "nothing to check" gate open -- a lossy trunc is
    // deliberately excluded from `has_implicit_trunc` (it can never
    // raise the error that flag exists to reach), so a program with
    // ONLY a lossy trunc and no other bounded thing anywhere would
    // otherwise skip the whole body walk before ever reaching the
    // `trunc` arm at all, making this test pass identically whether the
    // `lossy` check works or not -- caught live via bug-reintroduction
    // (disabling the arm's `lossy` gate alone did NOT fail this test
    // without `y` present, exactly the no-op-fix trap this arc has hit
    // before; confirmed it DOES fail once `y` forces the walk to run).
    let src = "\
module M {
    in a : [1]
    out b : [5] = 5
    reg y : [4] where y < 10 = 0

    Double(x : [n]) : [n + 2] { return x << 1 }

    rule step {
        a?
        b := Double(trunc.!(b))
    }
}
";
    let errors = run(src);
    assert!(errors.is_empty(), "errors: {errors:?}");
}

#[test]
fn shl_with_literal_amount_composes() {
    let src = "\
module M {
    reg total : [8] where total < 40 = 0
    reg i : [8] where i < 10 = 0
    rule step {
        total := (i << 1).!
    }
}
";
    let errors = run(src);
    assert!(errors.is_empty(), "errors: {errors:?}");
}

#[test]
fn shl_with_dynamic_amount_is_still_unprovable() {
    // Mirrors `Shl`'s own scope cut: a shift amount that isn't a
    // literal has no fixed scaling factor for the interval engine to
    // reason about, so it stays deliberately unrecognized.
    let src = "\
module M {
    reg total : [8] where total < 40 = 0
    reg i : [8] where i < 10 = 0
    reg k : [8] where k < 3 = 0
    rule step {
        total := i << k
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot verify this write"));
}

#[test]
fn trunc_that_already_fits_composes_as_identity() {
    let src = "\
module M {
    reg total : [8] where total < 40 = 0
    reg i : [8] where i < 10 = 0
    rule step {
        total := trunc(i, 4)
    }
}
";
    let errors = run(src);
    assert!(errors.is_empty(), "errors: {errors:?}");
}

#[test]
fn trunc_that_might_actually_truncate_falls_back_to_the_declared_width() {
    // `i`'s own declared range (`[0, 200)`) doesn't fit in 4 bits, so
    // `trunc`'s identity tier doesn't apply -- the honest fallback
    // (`[0, 16)`) is still real information, just not tight enough to
    // fit `total`'s own `< 10` bound.
    let src = "\
module M {
    reg total : [8] where total < 10 = 0
    reg i : [8] where i < 200 = 0
    rule step {
        total := trunc(i, 4)
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot verify this write"));
}

#[test]
fn declared_postcondition_still_takes_priority_over_inlining() {
    // When a callee declares an EXPLICIT return postcondition, it stays
    // authoritative even though the body would also be inline-able.
    // `Bump`'s declared bound (`_ < 30`, deliberately LOOSER than what
    // `i + 5` under `i < 10` actually proves, `[5, 15)`) is a real,
    // independently-checked-at-the-return-site fact, but still wider
    // than the tight per-call range inlining alone would compute. Two
    // calls composed via the DECLARED bound give `[0, 59)`, which
    // exceeds `total`'s own `< 44` bound and must be rejected -- if the
    // implementation silently preferred the tighter inlined result
    // instead (`(3+5) + (4+5) = [17, 18)`, well within `< 44`), this
    // would wrongly pass instead.
    let src = "\
module M {
    reg total : [8] where total < 44 = 0
    Bump(i : [8] where i < 10) : [8] where _ < 30 {
        return (i + 5).!
    }
    rule step {
        total := (Bump(3) + Bump(4)).!
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
        total := (unbounded + Bump(50)).!
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
fn else_branch_of_le_is_narrowed_to_gt() {
    // `else` of `i <= 5` is `i > 5`, i.e. lower bound `6`. Without that
    // fact, `total := i - 6` can't prove non-negativity (the sub arm
    // requires the smallest possible `i` to still dominate `6`).
    let src = "\
module M {
    reg total : [8] where total < 4 = 0
    reg i : [8] where i < 10 = 0
    rule step {
        if i <= 5 {
            total := 0
        } else {
            total := i - 6
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
fn body_substitution_inlining_does_not_poison_site_ranges_across_call_sites() {
    // Real bug found via advisor review: `Get`'s own body-substitution
    // inlining (called from `x := Get(3)` AND `y := Get(7)`) used to
    // reach `expr_bound`'s `Expr::Bracket` arm for `m[i]` TWICE, once
    // per call site, each time with `i` substituted to that call's OWN
    // literal argument -- and `site_ranges.insert` has no merge-on-
    // conflict, so the LAST call site's fact silently overwrote the
    // first for the SAME `ExprId` (the mem index lives inside `Get`'s
    // own body, shared textually across every caller). Canonically
    // (checking `Get`'s body directly, the way `check_item`'s own walk
    // does), `i` has no declared bound at all, so `site_ranges` should
    // have NO entry for this index -- any entry at all would be a
    // fabricated, call-site-specific "fact" reaching `schedule.rs`'s
    // own mem-disjointness proof as if it were a whole-program one.
    //
    // `x`/`y` carry a (deliberately unrelated, wide-open) `where` bound
    // purely so `check_item`'s own "nothing to check anywhere in the
    // program" fast path doesn't skip the whole body walk before
    // `expr_bound` ever runs -- confirmed load-bearing via bug-
    // reintroduction: an EARLIER version of this test left `x`/`y`
    // unbounded, which made `check_item` return before inlining was
    // ever reached at all, so the test passed regardless of whether the
    // fix was even present -- a no-op-fix, green-test false positive,
    // caught via advisor review before being trusted.
    let src = "\
module M {
    mem m : [8][16]
    reg x : [8] where x < 100 = 0
    reg y : [8] where y < 100 = 0
    Get(i : [8]) : [8] { return m[i] }
    rule a {
        x := Get(3)
    }
    rule b {
        y := Get(7)
    }
}
";
    let (fx, bounds, _errors) = run_with_bounds(src);
    // Not asserting on `errors` here: `Get`'s return VALUE composes to
    // `None` unconditionally (v17's own mem-read restriction — see
    // `Expr::Bracket`'s own doc comment), so `x`/`y`'s own write
    // obligation is expected to fail regardless of `site_ranges`. This
    // test is only about `site_ranges` staying clean.
    let index = the_only_mem_index(&fx);
    assert_eq!(bounds.site_ranges.get(&index), None);
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
fn struct_field_write_with_two_bounded_fields_checks_both() {
    // A regression pin for the `check_stmt`/shadow-check refactor that
    // collapsed three duplicated `found_writes`/`check_against_bound`
    // (resp. `shadow_compare`) call sites into one loop draining a
    // `Vec` of obligations: with TWO bounded fields on one struct write,
    // both obligations must still be checked, not just the first one
    // the (HashMap-ordered) field loop happens to push. Order-
    // independent on purpose -- `struct_field_bounds.keys()` iterates a
    // `HashMap`, so which field is checked first isn't, and never was,
    // a guaranteed order.
    let src = "\
module M {
    struct Pair {
        valid : [1] where _ < 1
        data : [8] where _ < 50
    }
    reg p : Pair = Pair{ valid: 0, data: 0 }
    rule write {
        p := Pair{ valid: 1, data: 60 }
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 2);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("field `valid`") && e.message.contains("could reach 1"))
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("field `data`") && e.message.contains("could reach 60"))
    );
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
            i := (i + 1).!
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

// --- `invariant` (relational bounds, DESIGN.md's "Tier 3, not v0") ---
//
// A whole separate mechanism from every scalar/mem/struct-field bound
// above: a fact about a COMBINATION of several `reg`/`out` defs,
// checked by its own dedicated induction (`check_relational_bound_
// induction` in bounds.rs) rather than the ordinary per-item `check_
// body` walk. `circular_buffer_disjoint.tr` (the FIFO push/pop example)
// is the real motivating case; these tests isolate each individual
// soundness gate with the smallest possible fixture.

#[test]
fn circular_buffer_disjoint_example_verifies_both_invariants() {
    // The actual driving example, verbatim: `push_count - pop_count <
    // 9` (occupancy never exceeds capacity) and `(head - tail -
    // push_count + pop_count) % 8 = 0` (the real FIFO pointer/counter
    // invariant) both check clean.
    let src = "\
module CircularBufferDisjoint {
    mem m : [8][8]
    reg head : [4] where head < 8 = 0
    reg tail : [4] where tail < 8 = 0
    reg push_count : [4] = 0
    reg pop_count : [4] = 0
    invariant push_count - pop_count < 9
    invariant (head - tail - push_count + pop_count) % 8 = 0
    in push_en : [1]
    in push_data : [8]
    in pop_en : [1]
    out pop_data : [8] = 0
    rule push {
        (push_en = 1)?
        (push_count - pop_count < 8)?
        m[head] := push_data
        if head < 7 {
            head := (head + 1).!
        } else {
            head := 0
        }
        push_count := (push_count + 1).!
    }
    rule pop {
        (pop_en = 1)?
        (push_count <> pop_count)?
        pop_data := m[tail]
        if tail < 7 {
            tail := (tail + 1).!
        } else {
            tail := 0
        }
        pop_count := (pop_count + 1).!
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn circular_buffer_weakened_guard_fails_the_occupancy_invariant() {
    // The load-bearing negative: this feature's own `m[i]` vs `m[2-i]`
    // analogue. Weakening `push`'s guard from `< 8` to `<= 8` lets
    // occupancy reach 8 -- `push_count - pop_count < 9` no longer
    // verifies, confirming the induction genuinely NEEDS the guard as a
    // hypothesis rather than trivially passing regardless.
    let src = "\
module M {
    reg head : [4] where head < 8 = 0
    reg tail : [4] where tail < 8 = 0
    reg push_count : [4] = 0
    reg pop_count : [4] = 0
    invariant push_count - pop_count < 9
    in push_en : [1]
    in pop_en : [1]
    rule push {
        (push_en = 1)?
        (push_count - pop_count <= 8)?
        push_count := (push_count + 1).!
    }
    rule pop {
        (pop_en = 1)?
        (push_count <> pop_count)?
        pop_count := (pop_count + 1).!
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("cannot verify this invariant is preserved")
    );
}

#[test]
fn invariant_only_module_with_no_other_bounded_def_still_verifies() {
    // Discriminates `check_item`'s own five-way early-return guard: `a`
    // has no `where` bound of its own, and nothing else in this module
    // is bounded either -- only `bump`'s own guard keeps `a - b`'s
    // induction sound. Not actually load-bearing for THIS feature's own
    // soundness (its induction is a separate pass, independent of `check
    // _item`), but pins the guard at five-way anyway, matching this
    // arc's own established discipline (see bounds.rs's own comment on
    // that guard).
    let src = "\
module M {
    reg a : [4] = 0
    reg b : [4] = 0
    invariant a - b < 9
    rule bump {
        (a - b < 8)?
        a := (a + 1).!
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn invariant_fails_with_no_guard_at_all() {
    // Without ANY guard limiting it, `a` increments forever -- `a - b`
    // eventually leaves the declared `[0, 9)` range. Confirms the
    // induction doesn't just trivially pass regardless of whether a
    // guard exists.
    let src = "\
module M {
    reg a : [4] = 0
    reg b : [4] = 0
    invariant a - b < 9
    rule bump {
        a := (a + 1).!
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("cannot verify this invariant is preserved")
    );
}

#[test]
fn invariant_modulus_not_dividing_native_width_is_rejected() {
    // Side-condition negative: 3 doesn't divide 2^4 = 16, so reducing a
    // mod-16 wrapping computation to mod 3 isn't congruence-preserving
    // -- rejected at declaration time, not silently mis-proven later.
    let src = "\
module M {
    reg a : [4] = 0
    reg b : [4] = 0
    invariant (a - b) % 3 < 2
    rule bump {
        a := (a + 1).!
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("must be a power of two dividing")
    );
}

#[test]
fn invariant_range_exceeding_its_own_modulus_is_rejected() {
    // Side-condition negative: a declared upper bound above the
    // modulus is meaningless (would accept anything once reduced) --
    // rejected at declaration time.
    let src = "\
module M {
    reg a : [4] = 0
    reg b : [4] = 0
    invariant a - b < 20
    rule bump {
        a := (a + 1).!
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("must fit within its own modulus")
    );
}

#[test]
fn invariant_write_in_only_one_if_branch_with_no_else_is_rejected() {
    // Post-ship advisor follow-up: `a`'s only write to a relevant def
    // sits inside an `if` with no `else` -- the TRUE delta is either
    // `+1` (branch taken) or `0` (branch skipped) depending on a runtime
    // input, and this pass has no per-path story for that; it must fail
    // closed rather than silently pick one delta (`deltas_agree` compares
    // the `then` branch's `{a: 1}` against the implicit `else` branch's
    // `{}` -- 1 and 0 disagree mod 16).
    let src = "\
module M {
    reg a : [4] = 0
    reg b : [4] = 0
    invariant a - b < 9
    in flag : [1]
    rule bump {
        (a - b < 8)?
        if flag = 1 {
            a := (a + 1).!
        }
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("cannot verify this invariant: rule")
    );
}

#[test]
fn invariant_verifies_when_a_rules_own_guards_are_jointly_unsatisfiable() {
    // Post-ship advisor follow-up: `bump`'s own two guards (`a - b < 3`
    // and `a - b > 5`) can never BOTH hold, so `narrow_combo_range`'s
    // sequential intersection collapses to an empty `(lo, hi)` with
    // `lo >= hi` -- `shift_preserves` correctly treats that as vacuously
    // safe (an unreachable rule proves anything), regardless of how
    // large the delta is. Paired with the NEXT test (the same delta
    // under a SATISFIABLE guard) to confirm this isn't just "the check
    // never runs" -- the delta genuinely would fail if reachable.
    let src = "\
module M {
    reg a : [4] = 0
    reg b : [4] = 0
    invariant a - b < 9
    rule bump {
        (a - b < 3)?
        (a - b > 5)?
        a := (a + 10).!
    }
}
";
    assert!(run(src).is_empty(), "{:?}", run(src));
}

#[test]
fn invariant_fails_with_the_same_delta_under_a_satisfiable_guard() {
    // The control for the test above: same `a := a + 10`, but with only
    // the FIRST guard (satisfiable on its own) -- confirms the induction
    // genuinely checks the arithmetic rather than always passing.
    let src = "\
module M {
    reg a : [4] = 0
    reg b : [4] = 0
    invariant a - b < 9
    rule bump {
        (a - b < 3)?
        a := (a + 10).!
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("cannot verify this invariant is preserved")
    );
}

#[test]
fn invariant_coefficient_other_than_plus_or_minus_one_is_rejected() {
    // v1 restriction: no scalar multiplication in an invariant's own
    // linear combination.
    let src = "\
module M {
    reg a : [4] = 0
    reg b : [4] = 0
    invariant 2 * a - b < 9
    rule bump {
        a := (a + 1).!
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("coefficient exactly +-1"));
}

#[test]
fn invariant_write_hidden_behind_a_call_is_rejected() {
    // Gate #1 (flagged before this feature was implemented): a rule
    // writing a named register only through a `fn`/`impl` call -- never
    // a directly-visible `Stmt::Assign` -- must fail this bound closed,
    // not silently compute a delta of zero for a write that's actually
    // there. Bug-reintroduction-style: this is exactly the shape that
    // WOULD silently pass if `walk_deltas`'s own findings weren't cross-
    // checked against `effects.rs`'s `sig.writes`.
    let src = "\
module M {
    reg a : [4] = 0
    reg b : [4] = 0
    invariant a - b < 9
    Bump() {
        a := (a + 1).!
    }
    rule bump {
        (a - b < 8)?
        Bump()
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("not through a directly-visible `Stmt::Assign`")
    );
}

#[test]
fn invariant_with_more_than_two_contributing_rules_is_rejected() {
    // v1 restriction: at most two rules may write registers a single
    // invariant names.
    let src = "\
module M {
    reg a : [4] = 0
    reg b : [4] = 0
    reg c : [4] = 0
    invariant a - b - c < 9
    rule bump_a {
        (a - b - c < 8)?
        a := (a + 1).!
    }
    rule bump_b {
        (a <> b)?
        b := b - 1
    }
    rule bump_c {
        (a <> c)?
        c := c - 1
    }
}
";
    let errors = run(src);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("more than two rules write registers")
    );
}

// --- `provably_disjoint_mem_indices` (the `schedule.rs` consumer half
// of DESIGN.md's "Tier 3, not v0" circular-buffer case) ---
//
// Tested directly here, independent of `schedule.rs`'s own plumbing,
// since the function only needs `Ast`/`Resolution`/`Types`/`Bounds`, an
// address width, and each side's own index `ExprId` plus its guards --
// exactly what these helpers build.

#[test]
fn circular_buffer_head_and_tail_are_provably_disjoint_under_joint_guards() {
    let src = "\
module CircularBufferDisjoint {
    mem m : [8][8]
    reg head : [4] where head < 8 = 0
    reg tail : [4] where tail < 8 = 0
    reg push_count : [4] = 0
    reg pop_count : [4] = 0
    invariant push_count - pop_count < 9
    invariant (head - tail - push_count + pop_count) % 8 = 0
    in push_en : [1]
    in push_data : [8]
    in pop_en : [1]
    out pop_data : [8] = 0
    rule push {
        (push_en = 1)?
        (push_count - pop_count < 8)?
        m[head] := push_data
        if head < 7 {
            head := (head + 1).!
        } else {
            head := 0
        }
        push_count := (push_count + 1).!
    }
    rule pop {
        (pop_en = 1)?
        (push_count <> pop_count)?
        pop_data := m[tail]
        if tail < 7 {
            tail := (tail + 1).!
        } else {
            tail := 0
        }
        pop_count := (pop_count + 1).!
    }
}
";
    let (ast, res, ty, bounds, errors) = run_full(src);
    assert!(errors.is_empty(), "{errors:?}");
    let push = rule_named(&ast, "push");
    let pop = rule_named(&ast, "pop");
    let (head_idx, head_guards, head_guards_neg) = the_only_mem_index_in_rule(&ast, &res, push);
    let (tail_idx, tail_guards, tail_guards_neg) = the_only_mem_index_in_rule(&ast, &res, pop);
    const ADDR_WIDTH: u64 = 3; // mem m : [8][8] -- clog2(8)
    assert!(bounds::provably_disjoint_mem_indices(
        &ast,
        &res,
        &ty,
        &bounds,
        ADDR_WIDTH,
        head_idx,
        &head_guards,
        &head_guards_neg,
        tail_idx,
        &tail_guards,
        &tail_guards_neg,
    ));
    // Order-independence: the caller may pass the pair either way.
    assert!(bounds::provably_disjoint_mem_indices(
        &ast,
        &res,
        &ty,
        &bounds,
        ADDR_WIDTH,
        tail_idx,
        &tail_guards,
        &tail_guards_neg,
        head_idx,
        &head_guards,
        &head_guards_neg,
    ));
}

#[test]
fn circular_buffer_push_count_and_pop_count_are_not_claimed_disjoint() {
    // Sanity check that this function isn't vacuously true for ANY
    // pair: `push_count`/`pop_count` themselves don't appear as the
    // `idx_a`/`idx_b` cancelling pair in either invariant (they're part
    // of the "other" side in the linking fact), so no rule of this
    // function's own shape applies.
    let src = "\
module CircularBufferDisjoint {
    mem m : [8][8]
    reg head : [4] where head < 8 = 0
    reg tail : [4] where tail < 8 = 0
    reg push_count : [4] = 0
    reg pop_count : [4] = 0
    invariant push_count - pop_count < 9
    invariant (head - tail - push_count + pop_count) % 8 = 0
    in push_en : [1]
    in push_data : [8]
    in pop_en : [1]
    out pop_data : [8] = 0
    rule push {
        (push_en = 1)?
        (push_count - pop_count < 8)?
        m[head] := push_data
        if head < 7 {
            head := (head + 1).!
        } else {
            head := 0
        }
        push_count := (push_count + 1).!
    }
    rule pop {
        (pop_en = 1)?
        (push_count <> pop_count)?
        pop_data := m[tail]
        if tail < 7 {
            tail := (tail + 1).!
        } else {
            tail := 0
        }
        pop_count := (pop_count + 1).!
    }
}
";
    let (ast, res, ty, bounds, errors) = run_full(src);
    assert!(errors.is_empty(), "{errors:?}");
    let push_count = any_ident_for(&ast, &res, def_named(&res, "push_count"));
    let pop_count = any_ident_for(&ast, &res, def_named(&res, "pop_count"));
    const ADDR_WIDTH: u64 = 3; // mem m : [8][8] -- clog2(8)
    assert!(!bounds::provably_disjoint_mem_indices(
        &ast,
        &res,
        &ty,
        &bounds,
        ADDR_WIDTH,
        push_count,
        &[],
        &[],
        pop_count,
        &[],
        &[],
    ));
}

#[test]
fn head_and_tail_not_disjoint_when_push_guard_is_weakened() {
    // The load-bearing negative: weaken `push`'s guard from `< 8` to
    // `<= 8` -- `push_count - pop_count < 9`'s OWN induction (see
    // `circular_buffer_weakened_guard_fails_the_occupancy_invariant`
    // above) already fails to verify under this change, so it never
    // reaches `bounds.relational` at all -- confirming the consumer
    // correctly has nothing to work with, rather than silently
    // reporting disjoint anyway.
    let src = "\
module CircularBufferDisjoint {
    mem m : [8][8]
    reg head : [4] where head < 8 = 0
    reg tail : [4] where tail < 8 = 0
    reg push_count : [4] = 0
    reg pop_count : [4] = 0
    invariant push_count - pop_count < 9
    invariant (head - tail - push_count + pop_count) % 8 = 0
    in push_en : [1]
    in push_data : [8]
    in pop_en : [1]
    out pop_data : [8] = 0
    rule push {
        (push_en = 1)?
        (push_count - pop_count <= 8)?
        m[head] := push_data
        if head < 7 {
            head := (head + 1).!
        } else {
            head := 0
        }
        push_count := (push_count + 1).!
    }
    rule pop {
        (pop_en = 1)?
        (push_count <> pop_count)?
        pop_data := m[tail]
        if tail < 7 {
            tail := (tail + 1).!
        } else {
            tail := 0
        }
        pop_count := (pop_count + 1).!
    }
}
";
    let (tokens, lex_errors) = lexer::lex(src);
    assert!(lex_errors.is_empty());
    let (ast, parse_errors) = parser::parse(src, &tokens);
    assert!(parse_errors.is_empty());
    let (res, resolve_errors) = resolve::resolve(&ast);
    assert!(resolve_errors.is_empty());
    let (fx, effect_errors) = effects::check(&ast, &res);
    assert!(effect_errors.is_empty());
    let (ty, type_errors) = types::check(&ast, &res, &fx);
    assert!(type_errors.is_empty());
    let (bounds, errors) = bounds::check(&ast, &res, &fx, &ty);
    assert_eq!(errors.len(), 1); // the occupancy invariant fails to verify
    let push = rule_named(&ast, "push");
    let pop = rule_named(&ast, "pop");
    let (head_idx, head_guards, head_guards_neg) = the_only_mem_index_in_rule(&ast, &res, push);
    let (tail_idx, tail_guards, tail_guards_neg) = the_only_mem_index_in_rule(&ast, &res, pop);
    const ADDR_WIDTH: u64 = 3; // mem m : [8][8] -- clog2(8)
    assert!(!bounds::provably_disjoint_mem_indices(
        &ast,
        &res,
        &ty,
        &bounds,
        ADDR_WIDTH,
        head_idx,
        &head_guards,
        &head_guards_neg,
        tail_idx,
        &tail_guards,
        &tail_guards_neg,
    ));
}
