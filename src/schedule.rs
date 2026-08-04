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
//! the scoped v1+v2 of DESIGN.md's "Arrays: one resource each" tier-3
//! proof — a syntactic affine-offset check living entirely in this
//! module, deliberately NOT a dependent/refinement type system: no new
//! types, no propositions, just one more index shape the same proof
//! recognizes. Sound only for read/write pairs (a mem's write port is
//! still one shared, priority-muxed port in emission — see
//! firrtl/module.rs — so two PROVEN-disjoint writers would still race on
//! it; write/write pairs stay fully conservative, unaffected by this).
//!
//! Two index shapes are recognized (`IndexForm` below); every index on
//! BOTH sides of a pair must recognize as one of them or the whole
//! proof fails closed:
//! - A bare compile-time-constant integer (`m[0]`) — two of these are
//!   provably different iff the constants themselves differ, e.g.
//!   `m[0] := x` and `y := m[1]` (v1).
//! - A plain state def (register/input/...) plus a compile-time-
//!   constant offset (`m[i]`, `m[i+1]`, `m[i-1]`) — never chased through
//!   a rule-local: a local can be reassigned mid-rule, and resolving
//!   through the wrong binding would be the exact reassigned-local/
//!   `Avg(Avg(x,y),z)` bug class this codebase has already shipped and
//!   fixed twice. Two of these are provably different only when BOTH
//!   name the SAME base def (a different register's value could
//!   coincide at runtime — `m[i]` vs `m[j]` for two distinct registers
//!   stays unprovable, by design, not an oversight) AND the mem's own
//!   depth is exactly a power of two. The latter is load-bearing, not
//!   caution for its own sake: address arithmetic wraps modulo the
//!   base's own width, and that modular argument is only sound when
//!   every representable address is a real, distinct memory cell — v0
//!   has no bounds check on an index against a non-power-of-two depth
//!   at all (an out-of-range index is currently undefined, left
//!   entirely to firtool), so this proof simply never depends on that
//!   undefined behavior rather than guessing at it (v2). Given both
//!   conditions, offsets are compared modulo 2^(the SMALLER of the
//!   mem's own address width and the base's own declared width) — `i +
//!   k` wraps at the base's own width (types.rs's modular-add rule),
//!   which can be narrower than the address width connected to the mem
//!   port, and two offsets differing mod the wider width can still
//!   alias mod the narrower one. The base's width must be a
//!   concretely-known `bits[N]` or the whole comparison fails closed.
//!
//!   The pre-edge-read invariant this whole affine argument leans on
//!   (both rules see the SAME value of a shared base within one cycle,
//!   regardless of which rule writes it) never needs a side condition
//!   checked here: if either rule also WRITES the base register, that
//!   register lands in the pair's own shared-state set alongside the
//!   mem, and `mem_disjoint`'s "every shared def must be this one mem"
//!   requirement rejects the whole pair outright — the case where the
//!   invariant would matter cannot reach this proof at all.
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

use crate::ast::{Ast, BinOp, Expr, ExprId, Item, ItemId, ScheduleDirective};
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
) -> (Schedule, Vec<ScheduleError>) {
    let mut scheduler = Scheduler {
        ast,
        res,
        fx,
        ty,
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
                let on: Vec<DefId> = ww
                    .iter()
                    .copied()
                    .chain(rw.iter().copied())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                let matched = exempt_sets
                    .iter()
                    .find(|(_, set, _)| set.contains(a) && set.contains(b));
                let exemption = matched.map_or(Exemption::None, |(kind, _, _)| *kind);
                // No user annotation, but every shared def is a mem whose
                // access sites provably touch different addresses: prove
                // it automatically rather than requiring `conflict_free`.
                // Scoped to ReadWrite pairs only (see this module's own
                // doc comment for why WriteWrite can't benefit the same
                // way in v0's emission model) and left alone if the user
                // already wrote an annotation of their own.
                let exemption = if exemption == Exemption::None
                    && kind == ConflictKind::ReadWrite
                    && self.mem_disjoint(&rw, sa, sb)
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

    /// Whether every def in `rw` (a pure read/write set — the caller
    /// only calls this when the pair's overall `ww` is empty, so no def
    /// here is written by both sides) is a `mem` whose access sites in
    /// `sa`/`sb` provably touch different addresses. A single non-mem
    /// def, or a mem def the proof can't close, fails the whole set —
    /// see this module's own doc comment.
    fn mem_disjoint(&self, rw: &BTreeSet<DefId>, sa: &EffectSig, sb: &EffectSig) -> bool {
        !rw.is_empty()
            && rw.iter().all(|def| {
                self.res.def(*def).kind == DefKind::Mem && self.one_mem_disjoint(*def, sa, sb)
            })
    }

    /// One mem def's own proof: find which side writes it (the other
    /// reads it, guaranteed by `mem_disjoint`'s caller), then check that
    /// side's recorded write-index sites against the other's read-index
    /// sites. Missing index data (a mem present in `reads`/`writes` with
    /// no recorded site) is treated as an unknown index, not "no
    /// access" — fails closed, never assumed disjoint.
    fn one_mem_disjoint(&self, def: DefId, sa: &EffectSig, sb: &EffectSig) -> bool {
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
        mem_accesses_disjoint(self.ast, self.res, self.ty, pow2_width, w_idx, r_idx)
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
                        "    index sites proven disjoint (constant addresses, or the same base \
                         plus a constant offset): no stall derived (no annotation needed)\n",
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
/// disjointness proof — see this module's own doc comment for exactly
/// what each variant means and when two of them are provably different.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexForm {
    /// A bare compile-time-constant integer.
    Const(u64),
    /// A plain state def (register/input/...) plus a compile-time
    /// constant offset — `m[i]` is `Affine(i, 0)`, `m[i+1]` is
    /// `Affine(i, 1)`, `m[i-1]` is `Affine(i, u64::MAX)` (the offset is
    /// already reduced modulo 2^64; the disjointness check reduces it
    /// again modulo the mem's own address width).
    Affine(DefId, u64),
}

/// Recognize an index expression as one of `IndexForm`'s two shapes, or
/// `None` if it's neither — never chases through a rule-local (a local
/// can be reassigned mid-rule; resolving through the wrong binding
/// would be the exact reassigned-local/`Avg(Avg(x,y),z)` bug class this
/// codebase has already shipped and fixed twice).
fn index_form(ast: &Ast, res: &Resolution, id: ExprId) -> Option<IndexForm> {
    match ast.expr(id) {
        Expr::Int(v) => Some(IndexForm::Const(*v)),
        Expr::SizedInt { value, .. } => Some(IndexForm::Const(*value)),
        Expr::Ident(_) => state_base(ast, res, id).map(|def| IndexForm::Affine(def, 0)),
        Expr::Binary {
            op: BinOp::Add,
            lhs,
            rhs,
        } => affine_operand(ast, res, *lhs, *rhs).or_else(|| affine_operand(ast, res, *rhs, *lhs)),
        // Only `base - k`, never `k - base`: the latter negates the
        // base itself, not a translation of it, and isn't the same
        // "same runtime value, shifted by a known amount" shape at all.
        Expr::Binary {
            op: BinOp::Sub,
            lhs,
            rhs,
        } => {
            let base = state_base(ast, res, *lhs)?;
            let k = const_index(ast, *rhs)?;
            Some(IndexForm::Affine(base, 0u64.wrapping_sub(k)))
        }
        _ => None,
    }
}

/// `base_expr + offset_expr` (either operand order) -> `Affine(base
/// def, k)`, if `base_expr` is a bare state-def reference and
/// `offset_expr` folds to a constant.
fn affine_operand(
    ast: &Ast,
    res: &Resolution,
    base_expr: ExprId,
    offset_expr: ExprId,
) -> Option<IndexForm> {
    let base = state_base(ast, res, base_expr)?;
    let k = const_index(ast, offset_expr)?;
    Some(IndexForm::Affine(base, k))
}

/// The state def a bare `Expr::Ident` resolves to, if any — the
/// disjointness proof's only notion of "the same runtime value."
/// Deliberately requires the `Ident` shape explicitly (unlike
/// `effects.rs`'s own `state_def`, which doesn't need to since every
/// caller there already only reaches it from an Ident position): without
/// this check, `res.expr_defs` would also resolve an inst-port `Expr::
/// Field` base (e.g. `m[c.a + 1]`), which is sound on its own (a port's
/// value is just as pre-edge-stable within a cycle) but would recognize
/// asymmetrically — `m[c.a + 1]` matching while a bare `m[c.a]` (offset
/// 0, the `Expr::Ident` match arm in `index_form`) does not.
fn state_base(ast: &Ast, res: &Resolution, id: ExprId) -> Option<DefId> {
    if !matches!(ast.expr(id), Expr::Ident(_)) {
        return None;
    }
    let def = res.expr_defs.get(&id)?;
    res.def(*def).kind.is_state().then_some(*def)
}

/// Fold a mem-index expression to a compile-time constant, if it is
/// one.
fn const_index(ast: &Ast, id: ExprId) -> Option<u64> {
    match ast.expr(id) {
        Expr::Int(v) => Some(*v),
        Expr::SizedInt { value, .. } => Some(*value),
        _ => None,
    }
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
/// differ. An affine form differs from another only when BOTH name the
/// same base def (a different register's value could coincide at
/// runtime), `pow2_width` is available (the mem's depth is a power of
/// two — see this module's own doc comment for why that's required, not
/// just cautious), AND the offsets differ modulo 2^width. That last
/// width is NOT simply the mem's own address width: `i + k` wraps at
/// the BASE's own declared width (types.rs's modular-add rule), which
/// can be narrower than the address width connected to the mem port —
/// two offsets differing mod the (wider) address width can still be
/// congruent, and therefore the same real address, mod the (narrower)
/// base width. Using `min(addr width, base width)` is sound either way:
/// congruence in the smaller modulus implies congruence in the larger
/// one it divides. The base's own width must be concretely known or the
/// whole comparison fails closed — a constant compared against an
/// affine form (or the reverse) is never provable.
fn forms_differ(ty: &Types, a: IndexForm, b: IndexForm, pow2_width: Option<u64>) -> bool {
    match (a, b) {
        (IndexForm::Const(x), IndexForm::Const(y)) => x != y,
        (IndexForm::Affine(da, ka), IndexForm::Affine(db, kb)) => {
            da == db
                && pow2_width.is_some_and(|addr_w| {
                    base_width(ty, da)
                        .is_some_and(|base_w| offsets_differ(ka, kb, addr_w.min(base_w)))
                })
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
/// the whole proof closed (unknown, not "assumed disjoint").
fn mem_accesses_disjoint(
    ast: &Ast,
    res: &Resolution,
    ty: &Types,
    pow2_width: Option<u64>,
    a: &BTreeSet<ExprId>,
    b: &BTreeSet<ExprId>,
) -> bool {
    let fold = |set: &BTreeSet<ExprId>| -> Option<Vec<IndexForm>> {
        set.iter().map(|e| index_form(ast, res, *e)).collect()
    };
    let (Some(a_forms), Some(b_forms)) = (fold(a), fold(b)) else {
        return false;
    };
    a_forms
        .iter()
        .all(|x| b_forms.iter().all(|y| forms_differ(ty, *x, *y, pow2_width)))
}
