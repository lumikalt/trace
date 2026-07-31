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
//! one resource) — trusted, NOT checked: v0 has no way to prove or
//! check address disjointness (that's the tier-3 banked-array proof
//! DESIGN.md defers), so there is nothing sound to assert for this one;
//! only the derived stall is waived. Claiming either for a pair that
//! does not conflict is legal overstatement.
//!
//! Rules conflict only within their own scope (module body or top level):
//! state is scope-local, so cross-scope conflicts cannot exist.

use crate::ast::{Ast, Item, ItemId, ScheduleDirective};
use crate::effects::Effects;
use crate::lexer::Span;
use crate::resolve::{DefId, Resolution};
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
    /// `conflict_free { a, b }` — trusted, unchecked in v0.
    ConflictFree,
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

pub fn schedule(ast: &Ast, res: &Resolution, fx: &Effects) -> (Schedule, Vec<ScheduleError>) {
    let mut scheduler = Scheduler {
        ast,
        res,
        fx,
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
                    .chain(rw)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                let matched = exempt_sets
                    .iter()
                    .find(|(_, set, _)| set.contains(a) && set.contains(b));
                let exemption = matched.map_or(Exemption::None, |(kind, _, _)| *kind);
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
                    Exemption::ConflictFree => out.push_str(
                        "    claimed conflict_free: no stall derived (trusted, not checked)\n",
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
