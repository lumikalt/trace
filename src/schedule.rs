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
//! disjointness (`Exemption::Disjoint`, `one_mem_disjoint` below). This
//! is DESIGN.md's "Arrays: one resource each" tier-3 proof, stage 4 of
//! the dependent-type-system rollout: a single generic Z3 query
//! (`bounds::provably_disjoint_mem_indices`, `bounds/smt.rs`) replaces
//! what used to be eight hand-built syntactic disjointness arguments
//! (`IndexForm`/`forms_differ`, kept in this module's own git history —
//! not duplicated here as prose; DESIGN.md has the retired design's full
//! rationale). Bitvector arithmetic natively subsumes the affine/
//! banking/mirrored-parity/relational reasoning those arguments existed
//! to hand-encode, so this module's own job shrinks to exactly two
//! things Z3 itself can't know: which index expressions were accessed
//! under which guards (`bounds::guarded_mem_accesses`, walking each
//! rule's own body), and the mem's real address width (`clog2(depth)
//! .max(1)`, `real_range` below — matches `firrtl/module.rs`'s own
//! `addr_w` exactly, since the Z3 query's own soundness depends on
//! comparing at the SAME width the real `mem.addr` port is declared at,
//! not an arbitrary one — see `bounds::provably_disjoint`'s own doc
//! comment for why). Sound only for read/write pairs (a mem's write
//! port is still one shared, priority-muxed port in emission — see
//! firrtl/module.rs — so two PROVEN-disjoint writers would still race on
//! it; write/write pairs stay fully conservative, unaffected by this).
//!
//! Every index on BOTH sides of a pair must translate to Z3 (`bounds
//! ::smt`'s `translate_expr`, the SAME translation `bounds.rs`'s own
//! obligation checks use) or the whole proof fails closed: unknown, not
//! "assumed disjoint." Claiming `mutually_exclusive`/`conflict_free` on
//! a pair that does not conflict (whether because it never did, or
//! because this proof now clears it) is legal overstatement.
//!
//! Rules conflict only within their own scope (module body or top level):
//! state is scope-local, so cross-scope conflicts cannot exist.
//!
//! `provably_disjoint_mem_defs` (renamed from `mem_disjoint`) is PER-DEF,
//! not all-or-nothing: `examples/circular_buffer_disjoint.tr`'s `push`/
//! `pop` share `{m, push_count, pop_count}`, and a mem def proven
//! disjoint drops out of the reported `on` set — UNLESS doing so would
//! empty it entirely, in which case `on` keeps the FULL set (matching
//! every prior release's own diagnostic convention: `Exemption::
//! Disjoint` says "here's what was checked," not just "here's what's
//! still unresolved"). `push_count`/`pop_count` are ordinary scalar
//! regs, not mem, so they're never candidates for this drop at all — the
//! pair's exemption stays `None` and the derived stall persists exactly
//! as before, with `m` simply no longer named alongside it.

use crate::ast::{Ast, Item, ItemId, ScheduleDirective};
use crate::bounds::Bounds;
use crate::effects::{EffectSig, Effects};
use crate::lexer::Span;
use crate::resolve::{DefId, DefKind, Resolution};
use crate::types::{Ty, Types};
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
    /// reads it, guaranteed by the caller), then check every write-index
    /// site against every read-index site via `bounds::provably_
    /// disjoint_mem_indices` — one Z3 query per pair, each paired with
    /// the guards active at its own access site (`bounds::guarded_mem_
    /// accesses`, walking the accessing rule's own body). Missing index
    /// data (a mem present in `reads`/`writes` with no recorded site) or
    /// an unresolvable depth is treated as unknown, not "no access" —
    /// fails closed, never assumed disjoint. A pair with no entry in the
    /// guard map (an access this module's own v1 scope doesn't reach,
    /// e.g. inside a `while`) falls back to no guards at all — fewer
    /// hypotheses, never a false disjointness claim.
    fn one_mem_disjoint(
        &self,
        def: DefId,
        sa: &EffectSig,
        sb: &EffectSig,
        rule_a: ItemId,
        rule_b: ItemId,
    ) -> bool {
        let (writer, reader, writer_rule, reader_rule) = if sa.writes.contains(&def) {
            (sa, sb, rule_a, rule_b)
        } else {
            (sb, sa, rule_b, rule_a)
        };
        let Some(w_idx) = writer.mem_write_idx.get(&def) else {
            return false;
        };
        let Some(r_idx) = reader.mem_read_idx.get(&def) else {
            return false;
        };
        let Some(depth) = self.real_depth(def) else {
            return false;
        };
        let addr_width = clog2(depth).max(1);
        let (
            Item::Rule {
                body: writer_body, ..
            },
            Item::Rule {
                body: reader_body, ..
            },
        ) = (self.ast.item(writer_rule), self.ast.item(reader_rule))
        else {
            return false;
        };
        let writer_guards = crate::bounds::guarded_mem_accesses(self.ast, self.res, writer_body);
        let reader_guards = crate::bounds::guarded_mem_accesses(self.ast, self.res, reader_body);
        w_idx.iter().all(|&wi| {
            let (wg, wgn) = writer_guards.get(&wi).cloned().unwrap_or_default();
            r_idx.iter().all(|&ri| {
                let (rg, rgn) = reader_guards.get(&ri).cloned().unwrap_or_default();
                crate::bounds::provably_disjoint_mem_indices(
                    self.ast,
                    self.res,
                    self.ty,
                    self.bounds,
                    addr_width,
                    wi,
                    &wg,
                    &wgn,
                    ri,
                    &rg,
                    &rgn,
                )
            })
        })
    }

    /// This mem's REAL, non-padded depth. `one_mem_disjoint` derives the
    /// mem's own address width from it (`clog2(depth).max(1)`, matching
    /// `firrtl/module.rs`'s own `addr_w` exactly) — the width the Z3
    /// disjointness query's own final comparison truncates to (`bounds
    /// ::provably_disjoint`'s own doc comment has the full soundness
    /// argument for why that specific width, not an arbitrary one).
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

/// `ceil(log2(v))`, `0` for `v <= 1` -- matches `firrtl/module.rs`'s own
/// `addr_w` exactly (that module's own `clog2`, a separate copy per this
/// codebase's existing precedent -- `elaborate.rs`, `types/mod.rs`, and
/// `lower/mod.rs` each already keep their own too), since `one_mem_
/// disjoint`'s address-width computation must land on the SAME width the
/// real `mem.addr` port is declared at for the Z3 disjointness query to
/// be sound (`bounds::provably_disjoint`'s own doc comment).
fn clog2(v: u64) -> u64 {
    if v <= 1 {
        0
    } else {
        64 - (v - 1).leading_zeros() as u64
    }
}
