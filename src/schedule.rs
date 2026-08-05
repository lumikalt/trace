//! Rule scheduling: the conflict matrix and urgency order from DESIGN.md.
//!
//! Two rules conflict when one writes state the other reads or writes
//! (read-read never conflicts). Conflicting rules cannot fire in the same
//! cycle; the more urgent rule wins and the other stalls for that cycle.
//!
//! Urgency comes from `schedule { urgency a > b }` directives, which form
//! a partial order; declaration order breaks ties. A cyclic urgency
//! specification is an error.
//!
//! Two annotations exempt a pair from the derived scheduling separation,
//! named to match Bluespec's own vocabulary (this project's cited
//! scheduling reference) rather than either sounding like the other:
//! `mutually_exclusive { a, b }` claims the two rules never both fire
//! the same cycle — checked, `Emitter` (firrtl/module.rs) emits a
//! FIRRTL `assert` for it. `conflict_free { a, b }` claims it's safe
//! for both to fire the same cycle (e.g. genuinely separate ports on
//! one resource) — trusted, NOT checked, for cases the compiler cannot
//! see into (an enable condition, a genuinely separate port the effect
//! system doesn't model as such). The one exception: when the pair
//! shares a `mem`, the claim's own precondition (the addresses actually
//! differ) IS observable — both are real compiled port signals — so
//! `Emitter` emits a checked assertion for it too, alongside the waived
//! stall, same spirit as `mutually_exclusive`'s (see firrtl/module.rs).
//!
//! A third case is neither user-directed nor trusted: a read/write pair
//! that both touch the same `mem` is auto-checked for address
//! disjointness (`Exemption::Disjoint`, `mem_disjoint` below). This is
//! the scoped v1+v2+v3 of DESIGN.md's "Arrays: one resource each" tier-3
//! proof — a syntactic check living entirely in this module, deliberately
//! NOT a dependent/refinement type system: no new types, no propositions.
//! `IndexForm` (below) is a single compositional representation — a
//! linear combination `multiplier * base + offset` of at most one state
//! def — built up by recursing through `+`/`-`/`*` via its own add/sub/
//! mul methods, rather than one enum variant or hardcoded pattern per
//! syntactic shape (v4): a new shape that still reduces to this same
//! linear form (nested arithmetic, sugar, whatever) is recognized by
//! composing the SAME rules, not by adding a new arm to `index_form`
//! every time one shows up — see that function's own doc comment.
//! Sound only for read/write pairs (a mem's write port is still one
//! shared, priority-muxed port in emission — see firrtl/module.rs — so
//! two PROVEN-disjoint writers would still race on it; write/write pairs
//! stay fully conservative, unaffected by this).
//!
//! `IndexForm` covers two provably-different shapes; every index on
//! BOTH sides of a pair must recognize as one of them or the whole
//! proof fails closed:
//! - A bare compile-time-constant integer (`m[0]`) — two of these are
//!   provably different iff the constants themselves differ, e.g.
//!   `m[0] := x` and `y := m[1]` (v1).
//! - A state def (register/input/...), optionally scaled by a compile-
//!   time-constant multiplier, plus a compile-time-constant offset
//!   (`m[i]`, `m[i+1]`, `m[i-1]`, `m[2*i]`, `m[2*i+1]`) — never chased
//!   through a rule-local: a local can be reassigned mid-rule, and
//!   resolving through the wrong binding would be the exact reassigned-
//!   local/`Avg(Avg(x,y),z)` bug class this codebase has already shipped
//!   and fixed twice. Two of these are provably different via EITHER of
//!   two independent arguments (both need the mem's own depth to be
//!   exactly a power of two — load-bearing, not caution for its own
//!   sake: v0 has no bounds check on an index against a non-power-of-
//!   two depth at all, an out-of-range index's behavior is undefined,
//!   left entirely to firtool, so neither argument may depend on it):
//!   - SAME base def AND SAME multiplier (v1/v2's original argument,
//!     unaffected by which multiplier is shared — it cancels exactly in
//!     the subtraction). Offsets are compared modulo 2^(the SMALLER of
//!     the mem's own address width and the base's own declared width):
//!     the base's own term wraps at ITS width (types.rs's modular-add
//!     rule), which can be narrower than the address width connected to
//!     the mem port, and two offsets differing mod the wider width can
//!     still alias mod the narrower one.
//!   - SAME power-of-two multiplier `M >= 2`, base identity IRRELEVANT
//!     (v3, the banking case): `m[i]` vs `m[j]` for two distinct
//!     registers stays unprovable on its own (a different register's
//!     value could coincide at runtime, and proving otherwise in
//!     general needs real range tracking — deliberately out of scope,
//!     not an oversight), but `m[2*i]` vs `m[2*j+1]` IS provable
//!     regardless of whether `i` and `j` coincide: `M*x` is congruent to
//!     0 mod `M` for ANY x, so base identity drops out of the argument
//!     entirely. Sound only up to `k = log2(M)` bits — see
//!     `forms_differ`'s own doc comment for exactly why (an empirical,
//!     not assumed, fact about how this compiles).
//!
//!   The pre-edge-read invariant the same-base argument leans on (both
//!   rules see the SAME value of a shared base within one cycle,
//!   regardless of which rule writes it) never needs a side condition
//!   checked here: if either rule also WRITES the base register, that
//!   register lands in the pair's own shared-state set alongside the
//!   mem, and `mem_disjoint`'s "every shared def must be this one mem"
//!   requirement rejects the whole pair outright — the case where the
//!   invariant would matter cannot reach this proof at all. The banking
//!   argument doesn't lean on this invariant at all — it doesn't care
//!   what either base's value is, or whether it changes.
//!
//! A CONSTANT compared against an AFFINE form (or the reverse) is never
//! provable either way — a fixed number says nothing about a variable's
//! possible runtime values. Any single index that doesn't recognize as
//! either shape fails the whole proof closed: unknown, not "assumed
//! disjoint." Claiming `mutually_exclusive`/`conflict_free` on a pair
//! that does not conflict (whether because it never did, or because
//! this proof now clears it) is legal overstatement.
//!
//! Rules conflict only within their own scope (module body or top level):
//! state is scope-local, so cross-scope conflicts cannot exist.
//!
//! # A fourth case: a proven value bound (`bounds.rs`)
//!
//! `reg i : [w] where i < K` (`bounds.rs`) statically proves `i`'s value
//! is ALWAYS in `[0, K)` — real integer arithmetic, no modulus, no
//! power-of-two-depth requirement. When both sides of a read/write pair
//! share the SAME base def and multiplier (the same structural
//! precondition the same-base argument above needs) and BOTH forms'
//! index expressions are independently confirmed to stay within the
//! mem's own REAL (non-padded) depth under that proven bound, the two
//! addresses differ by exactly `offset_a - offset_b` in plain integers
//! — no wraparound is possible, since the whole range is already proven
//! in-bounds, so there's nothing left to reason about mod anything. This
//! is a strictly SIMPLER argument than the same-base one above, not a
//! generalization of it: it just doesn't need the mem's depth to be a
//! power of two at all. See `forms_differ`'s own doc comment for the
//! exact gate, and `real_range`'s for why this is computed via an
//! INDEPENDENT walk rather than reusing `IndexForm`'s own (deliberately
//! wrapping) arithmetic.
//!
//! # A fifth case: two independently-bounded, UNRELATED bases
//!
//! `bounds.rs`'s `where` bound can also declare an explicit LOWER end
//! (`where L <= i < K`, `L` defaulting to 0). Two DIFFERENT registers,
//! each with its own proven `[L, K)` range, whose ranges don't overlap
//! at all (`m[i]` where `i`'s range is `[0,5)`, `m[j]` where `j`'s range
//! is `[5,10)`) can never address the same mem cell — this needs no
//! shared base, no shared multiplier, no relationship between `i` and
//! `j` whatsoever, only that their declared ranges are disjoint
//! intervals and both stay within the mem's real depth. This closes one
//! further, narrow slice of tier-3 (DESIGN.md's "Tier 3, not v0"): the
//! genuinely general case (two arbitrary, UNANNOTATED bases with no
//! declared range at all) stays exactly as unprovable as before —
//! nothing here infers a range from nothing, it only ever compares
//! ranges that were each independently declared and proven.
//!
//! # A sixth case: a per-SITE narrowed bound (v16), not just a flat
//! declared one
//!
//! The fourth/fifth cases above both read `Bounds.ranges`, a def's
//! FLAT, whole-program declared range — real, but often far wider than
//! what's actually true at one specific access site. `bounds.rs`'s own
//! forward walk proves a TIGHTER fact whenever a mem access sits under
//! a narrowing condition (`if i < 10 { m[i] := x }` proves `i < 10` at
//! THIS site, even when `i`'s own declared bound is far wider, `i <
//! 20`); v16 exports that per-site fact into `Bounds.site_ranges`,
//! keyed by the index expression's own `ExprId`, and `real_range` below
//! consults it first. Two regs each merely declared `i < 20`/`j < 20`
//! (individually insufficient — both ranges span the whole mem and
//! fully overlap) can still be proven disjoint this way if each is
//! narrowed to a different half by its OWN accessing rule's `if` guard
//! (`examples/mem_site_narrowing.tr`). Sound for exactly the same
//! reason the fourth/fifth cases are: every rule sees a shared reg's
//! IDENTICAL frozen, pre-edge value within one cycle (this module's own
//! "pre-edge-read invariant" above), so a narrowed fact proven at one
//! rule's own access site is just as true a fact about that shared
//! value as the def's flat declared range is — not a weaker, rule-local
//! claim.
//!
//! # A seventh case: a MIRRORED index (`examples/mirrored_index_
//! disjoint.tr`)
//!
//! Every case above reasons about either a SHARED symbolic term (same
//! base, same multiplier — v1-v3) or an independently-computed real
//! range (fourth/fifth/sixth). `m[i]` against `m[7 - i]` is neither: the
//! SAME base `i` appears on both sides, but negated on one — `IndexForm`
//! gained a `negated: bool` flag to represent `c - base` (a genuinely
//! different shape from `(-1)*base + c` composed the normal way, per
//! `IndexForm::sub`'s own doc comment) rather than trying to make
//! `multiplier` signed. `i` and `7 - i` can never coincide (mod any
//! power-of-two width, including wraparound) whenever `7` is ODD:
//! doubling a value and reducing mod a power of two can never produce
//! an odd result, so `2*i ≡ 7 (mod 2^W)` has no solution. This is a
//! PURELY ALGEBRAIC fact about the two index expressions themselves —
//! no state-write history involved at all, unlike a genuinely relational
//! invariant (DESIGN.md's own "Tier 3, not v0" section has exactly that:
//! `examples/circular_buffer_disjoint.tr`'s `head`/`tail`/`push_count`/
//! `pop_count` joint write history — see "An eighth case" below). Every
//! EXISTING
//! argument above had to be re-checked for a `negated`-shaped blind
//! spot: the symbolic ones (v1-v3, and the fourth case) all assumed a
//! shared term cancels exactly in the subtraction, which is false when
//! one side is negated (it DOUBLES instead) — found and fixed by adding
//! an `a.negated == b.negated` requirement to `same_base_and_multiplier`,
//! confirmed load-bearing by constructing a genuinely colliding pair
//! (`m[i]` vs `m[2 - i]`, real collision at `i = 1`) that the unguarded
//! code wrongly proved disjoint before the guard was added. The
//! range-based arguments (fourth/fifth/sixth) needed NO such guard: they
//! reason over achievable value SETS computed independently by
//! `bounds.rs`'s own interval arithmetic, and a real collision point is
//! by construction inside both sides' soundly-computed ranges, so a
//! range-overlap test can't misfire regardless of symbolic shape — see
//! `forms_differ`'s own doc comment for the full argument and its own
//! empirical confirmation (`i < 2`, narrow enough that `bounds.rs`
//! proves a real range for `2 - i` too, still correctly unprovable).
//!
//! # An eighth case: a genuinely RELATIONAL fact (`bounds::provably_
//! disjoint_under_joint_guards`)
//!
//! Every case above (v1-v7) reasons about the two index expressions
//! THEMSELVES — a shared symbolic term, an independently-computed range,
//! an algebraic parity fact. `examples/circular_buffer_disjoint.tr`'s
//! `m[head]` vs `m[tail]` is different in kind: `head`/`tail` are two
//! COMPLETELY UNRELATED bases (no shared base, no shared multiplier, no
//! individually-provable sub-range — each independently ranges over the
//! mem's FULL `[0, 8)` address space), disjoint ONLY because of a
//! RELATIONAL invariant across FOUR registers' joint write history
//! (`head - tail ≡ push_count - pop_count`, mod depth) — this is
//! DESIGN.md's "Tier 3, not v0" case, and none of v1-v7 can express it:
//! every one of them is a claim about one or two registers' own possible
//! VALUES, never about a fact relating them to OTHER registers entirely.
//!
//! `bounds::provably_disjoint_under_joint_guards` (defined in `bounds
//! .rs`, not here — the modular-arithmetic reasoning stays where the
//! rest of this arc's induction machinery already lives) is called as an
//! `||` alternative whenever `forms_differ` itself returns false, scoped
//! to bare-ident indices only (`multiplier == 1`, `offset == 0`, not
//! `negated` — no scaled/offset transform, since the underlying fact is
//! about the bases' own raw values). It consumes `Bounds.relational` —
//! `bounds.rs`'s own VERIFIED (not merely declared) `invariant` facts —
//! plus BOTH accessing rules' own leading guards, re-derived here rather
//! than cached from `bounds.rs`'s internal induction. See that function's
//! own doc comment for the full derivation.
//!
//! This case changed `mem_disjoint` (renamed `provably_disjoint_mem_
//! defs`) from ALL-OR-NOTHING to PER-DEF: `circular_buffer_disjoint.tr`'s
//! `push`/`pop` share `{m, push_count, pop_count}`, and the old gate
//! (`rw.iter().all(|def| kind == Mem && ...)`) rejected the WHOLE set the
//! instant it saw a non-mem def, meaning `m`'s own index proof was never
//! even ATTEMPTED regardless of whether this eighth argument existed —
//! confirmed empirically (not assumed) by deleting `m` from a scratch
//! copy of the example and observing the SAME `{push_count, pop_count}`
//! conflict reported either way. Now, a mem def proven disjoint drops out
//! of the reported `on` set — UNLESS doing so would empty it entirely, in
//! which case `on` keeps the FULL set (matching every prior release's own
//! diagnostic convention: `Exemption::Disjoint` says "here's what was
//! checked," not just "here's what's still unresolved" — an initial
//! version that always subtracted broke six EXISTING examples' own
//! `{m}` → `{}` diagnostic text, caught by the full-suite byte-diff sweep
//! before this shipped). `push_count`/`pop_count` are ordinary scalar
//! regs, not mem, so they're never candidates for this drop at all — the
//! pair's exemption stays `None` and the derived stall persists exactly
//! as before, with `m` simply no longer named alongside it.

use crate::ast::{Ast, BinOp, Expr, ExprId, Item, ItemId, ScheduleDirective};
use crate::bounds::Bounds;
use crate::effects::{EffectSig, Effects};
use crate::lexer::Span;
use crate::resolve::{DefId, DefKind, Resolution};
use crate::types::{Ty, Types, Width};
use std::collections::{BTreeSet, HashMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    WriteWrite,
    ReadWrite,
}

/// Whether (and how) a conflicting pair's derived stall was waived by a
/// schedule annotation — see this module's own doc comment for what
/// each claims and whether it's checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exemption {
    /// No annotation; the derived stall applies.
    None,
    /// `mutually_exclusive { a, b }` — checked via a simulation assertion.
    MutuallyExclusive,
    /// `conflict_free { a, b }` — trusted, unchecked in v0 for any
    /// non-mem shared state. For a read/write pair sharing a mem, the
    /// claim's own precondition (the two addresses actually differ) is
    /// checkable and DOES get a simulation assertion — see
    /// `firrtl/module.rs`'s emission of `conflict_free_mem_check_N`.
    ConflictFree,
    /// Auto-derived, not user-written: every shared mem access site on
    /// this read/write pair provably touches a different address — see
    /// this module's own doc comment for exactly which index shapes
    /// that covers. Proven, so unlike `ConflictFree` it needs no
    /// simulation check either (there is nothing left to trust).
    Disjoint,
}

impl Exemption {
    pub fn is_exempted(self) -> bool {
        self != Exemption::None
    }
}

#[derive(Debug, Clone)]
pub struct Conflict {
    pub a: ItemId,
    pub b: ItemId,
    /// The shared state driving the conflict.
    pub on: Vec<DefId>,
    pub kind: ConflictKind,
    /// Which claim, if any, waived the derived stall between this pair.
    pub exemption: Exemption,
    /// The rule that fires when both are ready (more urgent).
    pub winner: ItemId,
}

#[derive(Debug, Clone)]
pub struct GroupSchedule {
    /// The module owning this scope; None for top level.
    pub module: Option<ItemId>,
    /// Rules in final urgency order, most urgent first.
    pub order: Vec<ItemId>,
    pub conflicts: Vec<Conflict>,
    /// True when any `urgency` directive shaped the order.
    pub directed: bool,
}

#[derive(Debug, Default)]
pub struct Schedule {
    pub groups: Vec<GroupSchedule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleError {
    pub span: Span,
    pub message: String,
}

/// The span covering an exemption directive's whole name list, from the
/// first name to the last — used to underline `conflict_free { a, b }`
/// (not just one name in it) when the pair turns out to be a WriteWrite
/// conflict.
fn directive_span(names: &[crate::ast::Name]) -> Span {
    let start = names.first().map_or(0, |n| n.span.start);
    let end = names.last().map_or(start, |n| n.span.end);
    start..end
}

pub fn schedule(
    ast: &Ast,
    res: &Resolution,
    fx: &Effects,
    ty: &Types,
    bounds: &Bounds,
) -> (Schedule, Vec<ScheduleError>) {
    let mut scheduler = Scheduler {
        ast,
        res,
        fx,
        ty,
        bounds,
        out: Schedule::default(),
        errors: Vec::new(),
    };
    scheduler.group(None, &ast.roots.clone());
    (scheduler.out, scheduler.errors)
}

struct Scheduler<'a> {
    ast: &'a Ast,
    res: &'a Resolution,
    fx: &'a Effects,
    ty: &'a Types,
    bounds: &'a Bounds,
    out: Schedule,
    errors: Vec<ScheduleError>,
}

impl<'a> Scheduler<'a> {
    fn rule_name(&self, id: ItemId) -> &str {
        match self.ast.item(id) {
            Item::Rule { name, .. } => &name.text,
            _ => "?",
        }
    }

    fn group(&mut self, module: Option<ItemId>, items: &[ItemId]) {
        let mut rules: Vec<ItemId> = Vec::new();
        let mut schedules: Vec<ItemId> = Vec::new();
        for id in items {
            match self.ast.item(*id) {
                Item::Module {
                    items: inner_items, ..
                } => {
                    let inner = inner_items.clone();
                    self.group(Some(*id), &inner);
                }
                Item::Rule { .. } => rules.push(*id),
                Item::Schedule { .. } => schedules.push(*id),
                _ => {}
            }
        }
        if rules.is_empty() {
            return;
        }

        let by_name: HashMap<&str, ItemId> =
            rules.iter().map(|r| (self.rule_name(*r), *r)).collect();

        // Directives.
        let mut edges: Vec<(ItemId, ItemId)> = Vec::new();
        let mut exempt_sets: Vec<(Exemption, BTreeSet<ItemId>, Span)> = Vec::new();
        for sched in &schedules {
            let Item::Schedule { directives } = self.ast.item(*sched) else {
                continue;
            };
            for directive in directives {
                match directive {
                    ScheduleDirective::Urgency(names) => {
                        for pair in names.windows(2) {
                            let (Some(hi), Some(lo)) = (
                                by_name.get(pair[0].text.as_str()),
                                by_name.get(pair[1].text.as_str()),
                            ) else {
                                continue; // resolver reported unknown names
                            };
                            edges.push((*hi, *lo));
                        }
                    }
                    ScheduleDirective::MutuallyExclusive(names) => {
                        let set: BTreeSet<ItemId> = names
                            .iter()
                            .filter_map(|n| by_name.get(n.text.as_str()).copied())
                            .collect();
                        let span = directive_span(names);
                        exempt_sets.push((Exemption::MutuallyExclusive, set, span));
                    }
                    ScheduleDirective::ConflictFree(names) => {
                        let set: BTreeSet<ItemId> = names
                            .iter()
                            .filter_map(|n| by_name.get(n.text.as_str()).copied())
                            .collect();
                        let span = directive_span(names);
                        exempt_sets.push((Exemption::ConflictFree, set, span));
                    }
                }
            }
        }

        let directed = !edges.is_empty();
        let order = self.toposort(&rules, &edges, schedules.first().copied());
        let rank: HashMap<ItemId, usize> = order.iter().enumerate().map(|(i, r)| (*r, i)).collect();

        // Pairwise conflict matrix from the inferred rows.
        let mut conflicts = Vec::new();
        for (i, a) in rules.iter().enumerate() {
            for b in rules.iter().skip(i + 1) {
                let (Some(sa), Some(sb)) = (self.fx.sigs.get(a), self.fx.sigs.get(b)) else {
                    continue;
                };
                let ww: BTreeSet<DefId> = sa.writes.intersection(&sb.writes).copied().collect();
                let rw: BTreeSet<DefId> = sa
                    .writes
                    .intersection(&sb.reads)
                    .chain(sb.writes.intersection(&sa.reads))
                    .copied()
                    .collect();
                if ww.is_empty() && rw.is_empty() {
                    continue;
                }
                let kind = if ww.is_empty() {
                    ConflictKind::ReadWrite
                } else {
                    ConflictKind::WriteWrite
                };
                let full_on: BTreeSet<DefId> =
                    ww.iter().copied().chain(rw.iter().copied()).collect();
                // Any mem def in `rw` whose own access sites provably
                // touch different addresses is a candidate to drop from
                // the REPORTED `on` set -- but only when the pair, taken
                // as a whole, still has a REAL remaining hazard: when
                // proving every mem def disjoint also empties the WHOLE
                // set (today's existing `Exemption::Disjoint` case,
                // unchanged), `on` keeps showing the FULL originally-
                // shared set, matching every prior release's own
                // diagnostic convention (informational -- "here's what
                // was checked," not just "here's what's still a
                // problem"). Scoped to ReadWrite pairs only (see this
                // module's own doc comment for why WriteWrite can't
                // benefit the same way in v0's emission model).
                let provably_disjoint: BTreeSet<DefId> = if kind == ConflictKind::ReadWrite {
                    self.provably_disjoint_mem_defs(&rw, sa, sb, *a, *b)
                } else {
                    BTreeSet::new()
                };
                let fully_resolved = kind == ConflictKind::ReadWrite
                    && !rw.is_empty()
                    && full_on.difference(&provably_disjoint).next().is_none();
                let on: Vec<DefId> = if fully_resolved {
                    full_on.into_iter().collect()
                } else {
                    full_on.difference(&provably_disjoint).copied().collect()
                };
                let matched = exempt_sets
                    .iter()
                    .find(|(_, set, _)| set.contains(a) && set.contains(b));
                let exemption = matched.map_or(Exemption::None, |(kind, _, _)| *kind);
                // No user annotation, but every shared def is either a
                // proven-disjoint mem access or (today's existing case)
                // the set is entirely mem and entirely proven: exempt
                // automatically rather than requiring `conflict_free`,
                // left alone if the user already wrote an annotation of
                // their own.
                let exemption = if exemption == Exemption::None
                    && kind == ConflictKind::ReadWrite
                    && fully_resolved
                {
                    Exemption::Disjoint
                } else {
                    exemption
                };
                // `conflict_free` ("safe to fire concurrently") has no
                // meaning for a WriteWrite conflict in v0's emission
                // model: there is one shared writer port/connect target,
                // not two independent ones to be disjoint on, so both
                // rules firing the same cycle would be a silent
                // last-connect race with no way to check or arbitrate it
                // (see DESIGN.md's "Scheduling" section). Reject outright
                // rather than let it compile to a footgun; `mutually_
                // exclusive` is the correct annotation for a write-write
                // pair claimed to never actually coincide.
                if exemption == Exemption::ConflictFree && kind == ConflictKind::WriteWrite {
                    let span = matched.unwrap().2.clone();
                    // Report only the genuinely both-written state (`ww`),
                    // not the full `on` set -- a mixed conflict (e.g. `a`
                    // writes {x,y}, `b` writes {x}, reads {y}) is still
                    // WriteWrite overall (on `x`), but `on` also includes
                    // `y`, which only one side writes; naming it here
                    // would misattribute a read-write hazard as a
                    // write-write one.
                    let ww_names: Vec<&str> =
                        ww.iter().map(|d| self.res.def(*d).name.as_str()).collect();
                    self.errors.push(ScheduleError {
                        span,
                        message: format!(
                            "`conflict_free` claims rule {} and rule {} are safe to fire the \
                             SAME cycle, but they both write shared state ({}) -- v0 has only \
                             one writer port/connect target per resource, so this would be a \
                             silent, unarbitrated race, not a safe concurrent access; use \
                             `mutually_exclusive` instead if they truly never coincide",
                            self.rule_name(*a),
                            self.rule_name(*b),
                            ww_names.join(", "),
                        ),
                    });
                }
                let winner = if rank[a] <= rank[b] { *a } else { *b };
                conflicts.push(Conflict {
                    a: *a,
                    b: *b,
                    on,
                    kind,
                    exemption,
                    winner,
                });
            }
        }

        self.out.groups.push(GroupSchedule {
            module,
            order,
            conflicts,
            directed,
        });
    }

    /// The SUBSET of `rw` (a pure read/write set — the caller only calls
    /// this when the pair's overall `ww` is empty, so no def here is
    /// written by both sides) that is BOTH a `mem` def AND provably
    /// touched at different addresses by `sa`/`sb`'s own access sites —
    /// per-def, not all-or-nothing: a non-mem def (an ordinary scalar
    /// write-meets-read hazard, unrelated to mem indexing at all) or a
    /// mem def the proof can't close is simply left OUT of the returned
    /// set, not treated as failing the whole batch. This is the change
    /// that makes DESIGN.md's "Tier 3" circular-buffer case's `bounds
    /// .rs` half actually OBSERVABLE: `push`/`pop`'s shared `{m,
    /// push_count, pop_count}` used to make the OLD all-or-nothing `mem
    /// _disjoint` bail before even attempting `m`'s own index proof
    /// (`push_count`/`pop_count` aren't mem-kind); this drops exactly
    /// `m` from the reported set once its own indices verify disjoint,
    /// leaving `{push_count, pop_count}` — the genuine, still-real
    /// scalar hazard — as the pair's own `on` set and derived stall.
    fn provably_disjoint_mem_defs(
        &self,
        rw: &BTreeSet<DefId>,
        sa: &EffectSig,
        sb: &EffectSig,
        rule_a: ItemId,
        rule_b: ItemId,
    ) -> BTreeSet<DefId> {
        rw.iter()
            .copied()
            .filter(|def| {
                self.res.def(*def).kind == DefKind::Mem
                    && self.one_mem_disjoint(*def, sa, sb, rule_a, rule_b)
            })
            .collect()
    }

    /// One mem def's own proof: find which side writes it (the other
    /// reads it, guaranteed by the caller), then check that side's
    /// recorded write-index sites against the other's read-index sites.
    /// Missing index data (a mem present in `reads`/`writes` with no
    /// recorded site) is treated as an unknown index, not "no access" —
    /// fails closed, never assumed disjoint.
    fn one_mem_disjoint(
        &self,
        def: DefId,
        sa: &EffectSig,
        sb: &EffectSig,
        rule_a: ItemId,
        rule_b: ItemId,
    ) -> bool {
        let (writer, reader) = if sa.writes.contains(&def) {
            (sa, sb)
        } else {
            (sb, sa)
        };
        let Some(w_idx) = writer.mem_write_idx.get(&def) else {
            return false;
        };
        let Some(r_idx) = reader.mem_read_idx.get(&def) else {
            return false;
        };
        let pow2_width = self.pow2_addr_width(def);
        let real_depth = self.real_depth(def);
        mem_accesses_disjoint(
            self.ast,
            self.res,
            self.ty,
            self.bounds,
            pow2_width,
            real_depth,
            w_idx,
            r_idx,
            rule_a,
            rule_b,
        )
    }

    /// This mem's address width, but ONLY when its depth is exactly a
    /// power of two — see this module's own doc comment for why the
    /// affine-offset proof needs that, not just a defensive check.
    fn pow2_addr_width(&self, mem: DefId) -> Option<u64> {
        match self.ty.state_tys.get(&mem) {
            Some(Ty::Mem { len, .. }) if len.is_power_of_two() => Some(len.trailing_zeros() as u64),
            _ => None,
        }
    }

    /// This mem's REAL, non-padded depth — unlike `pow2_addr_width`, no
    /// power-of-two filter at all. Used only by the proven-bound
    /// disjointness argument (this module's own doc comment, "A fourth
    /// case"), which needs the mem's actual depth rather than the
    /// padded address space `clog2` rounds up to — the whole point of
    /// that argument is working on a depth the OTHER two arguments
    /// can't (a non-power-of-two one).
    fn real_depth(&self, mem: DefId) -> Option<u64> {
        match self.ty.state_tys.get(&mem) {
            Some(Ty::Mem { len, .. }) => Some(*len),
            _ => None,
        }
    }

    /// Kahn's algorithm over urgency edges; declaration order breaks
    /// ties (the earliest ready rule is picked first). Falls back to
    /// declaration order on a cycle, with an error.
    fn toposort(
        &mut self,
        rules: &[ItemId],
        edges: &[(ItemId, ItemId)],
        error_at: Option<ItemId>,
    ) -> Vec<ItemId> {
        let mut indegree: HashMap<ItemId, usize> = rules.iter().map(|r| (*r, 0)).collect();
        let mut succ: HashMap<ItemId, Vec<ItemId>> = HashMap::new();
        for (hi, lo) in edges {
            succ.entry(*hi).or_default().push(*lo);
            *indegree.entry(*lo).or_default() += 1;
        }
        let mut order = Vec::new();
        let mut remaining: Vec<ItemId> = rules.to_vec();
        while !remaining.is_empty() {
            let Some(pos) = remaining.iter().position(|r| indegree[r] == 0) else {
                // Cycle.
                let span = error_at
                    .map(|s| self.ast.item_spans[s.0 as usize].clone())
                    .unwrap_or(0..0);
                let names: Vec<&str> = remaining.iter().map(|r| self.rule_name(*r)).collect();
                self.errors.push(ScheduleError {
                    span,
                    message: format!("urgency directives form a cycle among {}", names.join(", ")),
                });
                order.extend(remaining.iter().copied());
                break;
            };
            let rule = remaining.remove(pos);
            order.push(rule);
            for next in succ.get(&rule).cloned().unwrap_or_default() {
                *indegree.get_mut(&next).unwrap() -= 1;
            }
        }
        order
    }
}

impl Schedule {
    /// The tier-1 diagnostic from DESIGN.md: say what was derived and why.
    pub fn explain(&self, ast: &Ast, res: &Resolution) -> String {
        let mut out = String::new();
        for group in &self.groups {
            let scope = match group.module {
                Some(m) => match ast.item(m) {
                    Item::Module { name, .. } => format!("module {name}"),
                    _ => "?".to_string(),
                },
                None => "top level".to_string(),
            };
            out.push_str(&format!("{scope}:\n"));
            let names: Vec<&str> = group
                .order
                .iter()
                .map(|r| match ast.item(*r) {
                    Item::Rule { name, .. } => name.text.as_str(),
                    _ => "?",
                })
                .collect();
            let source = if group.directed {
                "schedule directive"
            } else {
                "declaration order; no directive given"
            };
            out.push_str(&format!("  urgency: {}   ({source})\n", names.join(" > ")));
            if group.conflicts.is_empty() {
                out.push_str("  no conflicts: all rules can fire every cycle\n");
                continue;
            }
            for c in &group.conflicts {
                let (a, b) = (rule_name(ast, c.a), rule_name(ast, c.b));
                let on: Vec<&str> = c.on.iter().map(|d| res.def(*d).name.as_str()).collect();
                let kind = match c.kind {
                    ConflictKind::WriteWrite => "both write",
                    ConflictKind::ReadWrite => "write meets read on",
                };
                out.push_str(&format!(
                    "  rule {a} conflicts with rule {b}: {kind} {{{}}}\n",
                    on.join(", ")
                ));
                match c.exemption {
                    Exemption::MutuallyExclusive => out.push_str(
                        "    claimed mutually_exclusive: no stall derived (checked in \
                         simulation)\n",
                    ),
                    Exemption::ConflictFree => {
                        // Whether the emitter (firrtl/module.rs) actually
                        // adds a runtime assertion for this pair depends
                        // on whether any shared def is a mem — plain
                        // registers/fifos stay fully trusted, no assert
                        // exists for those. `on` is exactly this pair's
                        // shared state, so it's checkable right here
                        // without needing the emitter's own per-read-site
                        // bookkeeping.
                        let checked = c.on.iter().any(|d| res.def(*d).kind == DefKind::Mem);
                        out.push_str(if checked {
                            "    claimed conflict_free: no stall derived (mem addresses \
                             checked in simulation; any other shared state stays trusted, \
                             not checked)\n"
                        } else {
                            "    claimed conflict_free: no stall derived (trusted, not \
                             checked)\n"
                        });
                    }
                    Exemption::Disjoint => out.push_str(
                        "    index sites proven disjoint (constant addresses, the same base \
                         plus a constant offset, a shared power-of-two multiplier, a proven \
                         value bound, or two independently proven disjoint ranges): no stall \
                         derived (no annotation needed)\n",
                    ),
                    Exemption::None => {
                        let loser = if c.winner == c.a { b } else { a };
                        let winner = rule_name(ast, c.winner);
                        out.push_str(&format!(
                            "    derived stall: {loser} fires only when {winner} is blocked or \
                             idle\n"
                        ));
                    }
                }
            }
        }
        out
    }
}

fn rule_name(ast: &Ast, id: ItemId) -> &str {
    match ast.item(id) {
        Item::Rule { name, .. } => &name.text,
        _ => "?",
    }
}

/// The shape a mem-index expression is recognized as, for the
/// disjointness proof: a linear combination `multiplier * base +
/// offset` of AT MOST one state def, `base: None` standing for a bare
/// compile-time constant (`offset` alone, `multiplier` unused). This is
/// a single, compositional representation, not one enum variant per
/// syntactic shape — see `index_form`'s own doc comment for why that
/// matters. `m[i]` is `{base: Some(i), multiplier: 1, offset: 0}`,
/// `m[2*i+1]` is `{base: Some(i), multiplier: 2, offset: 1}`, `m[3]` is
/// `{base: None, offset: 3}`. Two forms are provably different either
/// by SAME base + SAME multiplier (the v1/v2 argument, unaffected by
/// which multiplier — it cancels in the subtraction), or by SAME
/// power-of-two multiplier alone regardless of base identity (the
/// banking argument — see `forms_differ`).
///
/// `negated` (added for `examples/mirrored_index_disjoint.tr`'s own
/// gap): when `true`, this form represents `offset - multiplier * base`
/// instead of `multiplier * base + offset` — a genuinely different
/// shape from negating `multiplier` itself (`c - base` isn't `(-1)*base
/// + c` composed the normal way; it's `base` subtracted FROM a
/// constant, not a translation of `base`), which is why this is a
/// separate flag rather than a signed `multiplier`. Every EXISTING
/// disjointness argument in `forms_differ` requires both sides' own
/// `negated` flags to match before treating two forms as "the same
/// shape" — a negated and non-negated form sharing a base and
/// multiplier are NOT
/// interchangeable (`i` and `7 - i` share `base = i, multiplier = 1`
/// but are never equal, while `i` and `i` obviously always are), so
/// conflating them would be a real soundness bug, not just an
/// imprecision. See `forms_differ`'s own "mirrored/negated" argument
/// for the one case that specifically NEEDS `negated` to differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IndexForm {
    base: Option<DefId>,
    multiplier: u64,
    offset: u64,
    negated: bool,
}

impl IndexForm {
    fn constant(offset: u64) -> Self {
        IndexForm {
            base: None,
            multiplier: 0,
            offset,
            negated: false,
        }
    }

    fn base(def: DefId) -> Self {
        IndexForm {
            base: Some(def),
            multiplier: 1,
            offset: 0,
            negated: false,
        }
    }

    /// `self + other`, if representable as a single linear term. At
    /// most one side may carry a base — two DIFFERENT bases summed
    /// (`i + j`) is a genuinely two-variable expression this
    /// representation can't capture, so composition fails rather than
    /// silently dropping one side.
    fn add(self, other: Self) -> Option<Self> {
        match (self.base, other.base) {
            (None, None) => Some(Self::constant(self.offset.wrapping_add(other.offset))),
            (Some(_), None) => Some(Self {
                offset: self.offset.wrapping_add(other.offset),
                ..self
            }),
            (None, Some(_)) => other.add(self),
            (Some(_), Some(_)) => None,
        }
    }

    /// `self - other`. `base - k` is `base`'s own form with the offset
    /// shifted (unaffected by `negated` — a negated `base` translated by
    /// a further constant is still negated, just at a new offset). `k -
    /// base` NEGATES `other`'s form rather than composing it the normal
    /// way (`c - (m*base + o) = -m*base + (c - o)`) — flips `negated`
    /// rather than requiring `other` start out non-negated, so a second
    /// subtraction correctly un-negates a form that was already negated
    /// (`c - (d - base) = base + (c - d)`). `base1 - base2` (two
    /// DIFFERENT bases) is still two-variable and stays unrepresentable
    /// regardless of either side's `negated` flag.
    fn sub(self, other: Self) -> Option<Self> {
        match (self.base, other.base) {
            (None, None) => Some(Self::constant(self.offset.wrapping_sub(other.offset))),
            (Some(_), None) => Some(Self {
                offset: self.offset.wrapping_sub(other.offset),
                ..self
            }),
            (None, Some(_)) => Some(Self {
                base: other.base,
                multiplier: other.multiplier,
                offset: self.offset.wrapping_sub(other.offset),
                negated: !other.negated,
            }),
            (Some(_), Some(_)) => None,
        }
    }

    /// `self * other`. Only representable when at least one side is a
    /// pure constant — `base * base'` would be quadratic, not linear.
    fn mul(self, other: Self) -> Option<Self> {
        match (self.base, other.base) {
            (None, None) => Some(Self::constant(self.offset.wrapping_mul(other.offset))),
            (Some(_), None) => Some(Self {
                multiplier: self.multiplier.wrapping_mul(other.offset),
                offset: self.offset.wrapping_mul(other.offset),
                ..self
            }),
            (None, Some(_)) => other.mul(self),
            (Some(_), Some(_)) => None,
        }
    }
}

/// Recognize an index expression as an `IndexForm`, or `None` if it
/// isn't one — never chases through a rule-local (a local can be
/// reassigned mid-rule; resolving through the wrong binding would be
/// the exact reassigned-local/`Avg(Avg(x,y),z)` bug class this codebase
/// has already shipped and fixed twice). Recurses through `+`/`-`/`*`
/// via `IndexForm`'s own composition methods rather than pattern-
/// matching each syntactic shape (`base+k`, `M*base`, `M*base+k`, ...)
/// as its own case: a NEW shape that still reduces to "one base, known-
/// scaled, plus a constant" — nested arithmetic like `(i+1)*2`, sugar,
/// whatever — is handled automatically by composing the SAME rules,
/// not by adding a new match arm here every time one shows up.
fn index_form(ast: &Ast, res: &Resolution, id: ExprId) -> Option<IndexForm> {
    match ast.expr(id) {
        Expr::Int(v) => Some(IndexForm::constant(*v)),
        Expr::SizedInt { value, .. } => Some(IndexForm::constant(*value)),
        Expr::Ident(_) => state_base(ast, res, id).map(IndexForm::base),
        Expr::Binary { op, lhs, rhs } => {
            let a = index_form(ast, res, *lhs)?;
            let b = index_form(ast, res, *rhs)?;
            match op {
                BinOp::Add => a.add(b),
                BinOp::Sub => a.sub(b),
                BinOp::Mul => a.mul(b),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The state def a bare `Expr::Ident` resolves to, if any — the
/// disjointness proof's only notion of "the same runtime value".
/// Deliberately requires the `Ident` shape explicitly (unlike
/// `effects.rs`'s own `state_def`, which doesn't need to since every
/// caller there already only reaches it from an Ident position): this
/// function's own only caller (`index_form`'s `Expr::Ident` arm) already
/// guarantees the shape today, making the check structurally redundant
/// there — but it's left in as defense-in-depth, since it's what closed
/// a real asymmetry before the v4 compositional refactor (when a since-
/// removed helper called this from a Binary operand position, where
/// `res.expr_defs` would also resolve an inst-port `Expr::Field` base
/// like `c.a`, recognizing `m[c.a + 1]` while a bare `m[c.a]` did not).
fn state_base(ast: &Ast, res: &Resolution, id: ExprId) -> Option<DefId> {
    if !matches!(ast.expr(id), Expr::Ident(_)) {
        return None;
    }
    let def = res.expr_defs.get(&id)?;
    res.def(*def).kind.is_state().then_some(*def)
}

/// The declared bit width of a state def, if it's a plain `bits[N]`
/// (concretely known) — a register/input/output's own width. Anything
/// else (unknown width, or a non-scalar state kind like `Mem`/`Fifo`,
/// which can't legally be a bare index expression's base anyway) is
/// `None`, so the affine proof fails closed rather than guessing.
fn base_width(ty: &Types, def: DefId) -> Option<u64> {
    match ty.state_tys.get(&def) {
        Some(Ty::Bits(Width::Known(w))) => Some(*w),
        _ => None,
    }
}

/// Whether two recognized index forms are provably different. A
/// constant differs from another constant iff the values themselves
/// differ. An affine form differs from another via EITHER of two
/// independent sufficient arguments (an `||`, not a single combined
/// condition), both requiring `pow2_width` (the mem's depth is a power
/// of two — see this module's own doc comment for why that's required,
/// not just cautious):
///
/// - Same base AND same multiplier: the v1/v2 argument, unaffected by
///   which multiplier is shared — the `M*base` term is byte-identical on
///   both sides, so it cancels exactly in the subtraction regardless of
///   M's value, leaving "offsets differ" as the whole question, exactly
///   as if M were 1. Offsets are compared modulo 2^(the SMALLER of the
///   mem's own address width and the base's own declared width) — `base`
///   wraps at the base's own width (types.rs's modular-add rule), which
///   can be narrower than the address width connected to the mem port,
///   and two offsets differing mod the wider width can still alias mod
///   the narrower one.
/// - Same power-of-two multiplier M >= 2, base identity irrelevant (a
///   DIFFERENT register's value could still coincide at runtime, but
///   `M*x` is congruent to 0 mod M for ANY x — see this module's own doc
///   comment's "Arrays: one resource each" companion in DESIGN.md): the
///   banking argument. Sound only up to `k = log2(M)` bits: a compiled
///   `M*base [+ k]` expression's own natural width was checked via the
///   CLI (M = 2, 4, and 8 against a 4-bit base, not assumed from the
///   general width-growth rule) and consistently collapses to the
///   base's OWN declared width rather than growing to `|M|+|base|`, so
///   the low k bits survive every later truncation only when that
///   intermediate width — the base's own — is at least
///   k, hence the `base_width(..) >= k_used` guard on BOTH sides
///   (regardless of base identity). `k_used` is further capped at the
///   mem's own address width for the same reason the same-base argument
///   caps at it: only that many bits ultimately reach the port.
///
/// Either way, a def's own width must be concretely known or that
/// argument fails closed. A constant compared against an affine form
/// (or the reverse) is never provable.
///
/// A FOURTH, independent argument (this module's own doc comment, "A
/// fourth case") is tried before either pow2-gated argument above,
/// since it needs no power-of-two depth at all: same base AND same
/// multiplier (the identical structural precondition the same-base
/// argument needs — the shared `base*multiplier` term cancels exactly
/// in the subtraction, leaving `offset_a - offset_b`, real integers, as
/// the whole question), BOTH sides' real UPPER bound confirmed `<= ` the
/// mem's own REAL depth (so neither address can ever reach the
/// undefined out-of-range region), and the offsets genuinely differ. No
/// modulus, no wraparound reasoning needed at all — the range proof
/// already rules out wraparound occurring in the first place.
///
/// A FIFTH, independent argument (this module's own doc comment, "A
/// fifth case") is tried FIRST, before every other argument above,
/// including the fourth: BOTH sides' real RANGES (not just an upper
/// bound) are confirmed `<= ` the mem's own real depth, and the two
/// ranges themselves are provably non-overlapping intervals. This needs
/// no base-identity relationship at all — a constant, a bare bounded
/// reg, or an affine form of any base can each provide a real range, and
/// two non-overlapping ranges can never coincide in value regardless of
/// whether they share a base. It's strictly WEAKER than the fourth
/// argument for a genuinely same-base pair, though — `m[i]` vs `m[i+1]`
/// under `i < 9` has overlapping ranges (`[0,9)` vs `[1,10)`) even
/// though the fourth argument already proves it disjoint by offset
/// alone — so it never replaces that argument, only reaches a case
/// neither pow2-gated nor same-base argument can: two independently
/// bounded, unrelated bases (`schedule.rs`'s own "A fifth case" doc
/// comment has the full motivating example).
///
/// A SIXTH, independent argument (`examples/mirrored_index_disjoint.tr`'s
/// own driving case) needs neither `pow2_width` nor a proven range at
/// all: SAME base, both multipliers exactly 1, but OPPOSITE `negated`
/// (`base + oa` against `oc - base`). These coincide iff `2*base ≡ oc -
/// oa (mod 2^W)` for `base`'s own declared width `W` — and `2*base mod
/// 2^W` is ALWAYS even (doubling, then reducing mod a power of two,
/// can't touch bit 0), so there's no solution whenever `oc - oa` is
/// ODD. Unlike every argument above, this one needs no width GUARD at
/// all: bit 0 of a difference is invariant under truncation to any
/// narrower power-of-two width (truncation only drops HIGH bits), so
/// the parity check is correct whether it's evaluated at the base's own
/// declared width, the mem's address width, or plain `u64` wraparound —
/// confirmed via the CLI that a literal not fitting the base's own
/// declared width is a compile error (`1000 - i` against a 4-bit `i`
/// rejected outright), so `oc`/`oa` are always the SAME literal value
/// the base's own width would produce, never a wider-then-truncated one
/// the way the banking argument's multiplier is. This is why `negated`
/// must otherwise GATE every argument above it (`same_base_and_
/// multiplier` now requires `a.negated == b.negated`): without that
/// guard, `i` and `7 - i` (same base, same multiplier magnitude 1)
/// would wrongly look like "the same shape, offsets differ" to the
/// fourth/same-base arguments, an actual soundness bug, not just an
/// imprecision — the `M*base` term does NOT cancel in the subtraction
/// when one side is negated, it DOUBLES instead. The banking argument
/// is the one exception needing no such guard: its own claim (`M*x`
/// contributes 0 to the low `log2(M)` bits regardless of `x`) holds
/// identically whether that term is added or subtracted.
///
/// Why the FOURTH/FIFTH (range-based) arguments above need NO `negated`
/// guard at all, unlike the symbolic ones: they reason over ACHIEVABLE
/// VALUE SETS, computed independently by `bounds.rs`'s own interval
/// arithmetic (`real_range`/`site_ranges`, gated on genuine underflow
/// safety), not over a shared symbolic term that's assumed to cancel.
/// If `i` and `c - i` really do collide at some reachable `i = k`, then
/// `k` is by construction an achievable value of BOTH expressions, so
/// `k` lies inside both of their soundly-computed intervals — the
/// `a_hi <= b_lo || b_hi <= a_lo` disjointness test can never pass at a
/// point both ranges actually contain. Confirmed, not just argued: `m[i]`
/// vs `m[2 - i]` under a narrow `where i < 2` (small enough that
/// `bounds.rs`'s own `Sub` arm proves a real range for `2 - i` too) still
/// correctly fails to prove disjoint, even though `i`'s range `[0,2)`
/// and `2 - i`'s range `[1,3)` are each independently sound — they
/// overlap at `1`, the actual collision point, so the range argument
/// declines exactly where it must.
fn forms_differ(
    ty: &Types,
    a: IndexForm,
    b: IndexForm,
    pow2_width: Option<u64>,
    a_real_range: Option<(u64, u64)>,
    b_real_range: Option<(u64, u64)>,
    real_depth: Option<u64>,
) -> bool {
    let range_disjoint_argument = real_depth.is_some_and(|depth| {
        a_real_range
            .zip(b_real_range)
            .is_some_and(|((a_lo, a_hi), (b_lo, b_hi))| {
                a_hi <= depth && b_hi <= depth && (a_hi <= b_lo || b_hi <= a_lo)
            })
    });
    if range_disjoint_argument {
        return true;
    }
    match (a.base, b.base) {
        (None, None) => a.offset != b.offset,
        (Some(da), Some(db)) => {
            let mirrored_argument =
                da == db && a.multiplier == 1 && b.multiplier == 1 && a.negated != b.negated && {
                    let (neg, pos) = if a.negated { (a, b) } else { (b, a) };
                    neg.offset.wrapping_sub(pos.offset) & 1 == 1
                };
            if mirrored_argument {
                return true;
            }
            let same_base_and_multiplier =
                da == db && a.multiplier == b.multiplier && a.negated == b.negated;
            let proven_bound_argument = same_base_and_multiplier
                && a.offset != b.offset
                && real_depth.is_some_and(|depth| {
                    a_real_range.is_some_and(|(_, hi)| hi <= depth)
                        && b_real_range.is_some_and(|(_, hi)| hi <= depth)
                });
            if proven_bound_argument {
                return true;
            }
            let Some(addr_w) = pow2_width else {
                return false;
            };
            let same_base_argument = same_base_and_multiplier
                && base_width(ty, da)
                    .is_some_and(|base_w| offsets_differ(a.offset, b.offset, addr_w.min(base_w)));
            let banking_argument = a.multiplier == b.multiplier
                && a.multiplier >= 2
                && a.multiplier.is_power_of_two()
                && {
                    let k_used = (a.multiplier.trailing_zeros() as u64).min(addr_w);
                    base_width(ty, da).is_some_and(|w| w >= k_used)
                        && base_width(ty, db).is_some_and(|w| w >= k_used)
                        && offsets_differ(a.offset, b.offset, k_used)
                };
            same_base_argument || banking_argument
        }
        _ => false,
    }
}

/// `ka != kb` modulo 2^`width` (both already reduced modulo 2^64 by
/// `index_form`'s own wrapping arithmetic).
fn offsets_differ(ka: u64, kb: u64, width: u64) -> bool {
    let mask = if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    };
    (ka.wrapping_sub(kb)) & mask != 0
}

/// Every index in `a` is provably different from every index in `b` —
/// sound only when EVERY index on both sides recognizes as one of
/// `IndexForm`'s two shapes; a single unrecognized index anywhere fails
/// the whole proof closed (unknown, not "assumed disjoint"). Each index
/// is ALSO paired with its own `real_range` (independent of whether
/// `index_form` itself succeeds for it) — the fourth and fifth
/// disjointness arguments in `forms_differ` both need it.
// Each parameter is a genuinely distinct fact the proof needs (three
// proof-independent contexts — `ty`/`bounds` for width/bound lookups,
// `pow2_width`/`real_depth` gating four DIFFERENT sound-only-under-one-
// of-these-two-conditions arguments (two pow2-gated, two depth-gated) —
// plus the two access-site sets themselves); bundling them into a
// struct here alone would be inconsistent with `one_mem_disjoint`'s own
// equally-flat call one level up.
#[allow(clippy::too_many_arguments)]
fn mem_accesses_disjoint(
    ast: &Ast,
    res: &Resolution,
    ty: &Types,
    bounds: &Bounds,
    pow2_width: Option<u64>,
    real_depth: Option<u64>,
    a: &BTreeSet<ExprId>,
    b: &BTreeSet<ExprId>,
    rule_a: ItemId,
    rule_b: ItemId,
) -> bool {
    // An `IndexForm` paired with its own `real_range` (`None` when
    // either recognition fails) — named here purely to keep the
    // closure's return type readable, per clippy's own suggestion.
    type FormWithRange = (IndexForm, Option<(u64, u64)>);
    let fold = |set: &BTreeSet<ExprId>| -> Option<Vec<FormWithRange>> {
        set.iter()
            .map(|e| index_form(ast, res, *e).map(|f| (f, real_range(ast, res, bounds, *e))))
            .collect()
    };
    let (Some(a_forms), Some(b_forms)) = (fold(a), fold(b)) else {
        return false;
    };
    a_forms.iter().all(|(fa, ra)| {
        b_forms.iter().all(|(fb, rb)| {
            forms_differ(ty, *fa, *fb, pow2_width, *ra, *rb, real_depth)
                // The eighth, genuinely general argument (DESIGN.md's
                // "Tier 3, not v0"): two arbitrary, unrelated,
                // UNANNOTATED bases — `forms_differ`'s own v1-v7
                // arguments can never close this, since none of them
                // reasons about a RELATIONAL fact spanning defs outside
                // the two indices themselves. Scoped to BARE-IDENT
                // indices only (multiplier 1, offset 0, not negated —
                // no scaled/offset expression, since the exported fact
                // is about the bases' own raw values, not a transform
                // of them) and two DIFFERENT bases (same reasoning
                // `forms_differ`'s own `(Some(da), Some(db))` match arm
                // already keys off).
                || (fa.base != fb.base
                    && fa.multiplier == 1
                    && fa.offset == 0
                    && !fa.negated
                    && fb.multiplier == 1
                    && fb.offset == 0
                    && !fb.negated
                    && fa.base.zip(fb.base).is_some_and(|(da, db)| {
                        crate::bounds::provably_disjoint_under_joint_guards(
                            ast, res, bounds, da, db, rule_a, rule_b,
                        )
                    }))
        })
    })
}

/// The value range an index expression is provably confined to, as a
/// REAL (non-wrapping, non-modular) `(lower, upper)` pair — `Some((L,
/// K))` means the expression's value is ALWAYS in `[L, K)`, given
/// `bounds` (`bounds.rs`'s own proven ranges; see this module's own doc
/// comment's "A fourth case" and "A fifth case").
///
/// v16: first consults `bounds.site_ranges`, keyed by this exact
/// `ExprId` — a per-SITE fact `bounds.rs` proved during its own forward
/// walk, tighter than (or equal to) a bare def's flat declared range
/// whenever this specific index sits under a narrowing condition
/// (`if i < 10 { m[i] := x }` proves `i < 10` at THIS site, not just
/// `i`'s own whole-program declared bound). Falls back to the
/// INDEPENDENT walk below when that lookup misses (an `ExprId` this
/// pass never visited, or never found provable) — this fallback is
/// what keeps the whole proof fail-closed, not a replacement for it.
///
/// The independent walk itself is NOT derived from `IndexForm`:
/// `IndexForm`'s own `add`/`sub`/`mul` use `wrapping_*` arithmetic on
/// purpose (v1-v3's proofs reason mod 2^width), so `m[i-1]`'s
/// `IndexForm` stores its offset as a wrapped `u64::MAX` — treating
/// that as a real, non-negative integer would be simply wrong. Only a
/// bare bounded def, a literal, or `Add` of two such are recognized
/// here directly — `Sub`/`Mul`/`Call`/anything else falls to `None` in
/// THIS independent walk (stale claim, corrected: this no longer
/// matches `bounds.rs`'s own `expr_bound`, which gained `Sub` at v7 and
/// `Mul` at v11) — but for any `ExprId` `bounds.rs`'s own forward walk
/// already visited as a mem index, the `site_ranges` lookup above
/// already covers those wider shapes directly, so this independent
/// walk only needs to keep covering whatever `bounds.rs` never visited
/// (an index outside any mem access this pass reaches, or one it found
/// unprovable).
fn real_range(ast: &Ast, res: &Resolution, bounds: &Bounds, id: ExprId) -> Option<(u64, u64)> {
    if let Some(&range) = bounds.site_ranges.get(&id) {
        return Some(range);
    }
    match ast.expr(id) {
        Expr::Int(v) => Some((*v, v.checked_add(1)?)),
        Expr::SizedInt { value, .. } => Some((*value, value.checked_add(1)?)),
        Expr::Ident(_) => {
            let def = res.expr_defs.get(&id)?;
            bounds.ranges.get(def).copied()
        }
        Expr::Binary {
            op: BinOp::Add,
            lhs,
            rhs,
        } => {
            let (a_lo, a_hi) = real_range(ast, res, bounds, *lhs)?;
            let (b_lo, b_hi) = real_range(ast, res, bounds, *rhs)?;
            let lo = a_lo.checked_add(b_lo)?;
            let hi = a_hi.checked_add(b_hi)?.checked_sub(1)?;
            Some((lo, hi))
        }
        _ => None,
    }
}
