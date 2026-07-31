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
//! `conflict_free { a, b }` exempts a pair from scheduling separation.
//! The claim is recorded here (`Conflict::exempted`), not trusted: `Emitter`
//! (firrtl/module.rs) emits a FIRRTL `assert` for every exempted pair,
//! checking the two rules never both fire the same cycle. Claiming
//! conflict-freedom for a pair that does not conflict is legal
//! overstatement.
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

#[derive(Debug, Clone)]
pub struct Conflict {
    pub a: ItemId,
    pub b: ItemId,
    /// The shared state driving the conflict.
    pub on: Vec<DefId>,
    pub kind: ConflictKind,
    /// Claimed conflict-free; separation waived, assertion owed.
    pub exempted: bool,
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

pub fn schedule(ast: &Ast, _res: &Resolution, fx: &Effects) -> (Schedule, Vec<ScheduleError>) {
    let mut scheduler = Scheduler {
        ast,
        fx,
        out: Schedule::default(),
        errors: Vec::new(),
    };
    scheduler.group(None, &ast.roots.clone());
    (scheduler.out, scheduler.errors)
}

struct Scheduler<'a> {
    ast: &'a Ast,
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
        let mut exempt_sets: Vec<BTreeSet<ItemId>> = Vec::new();
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
                    ScheduleDirective::ConflictFree(names) => {
                        let set: BTreeSet<ItemId> = names
                            .iter()
                            .filter_map(|n| by_name.get(n.text.as_str()).copied())
                            .collect();
                        exempt_sets.push(set);
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
                    .into_iter()
                    .chain(rw)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                let exempted = exempt_sets
                    .iter()
                    .any(|set| set.contains(a) && set.contains(b));
                let winner = if rank[a] <= rank[b] { *a } else { *b };
                conflicts.push(Conflict {
                    a: *a,
                    b: *b,
                    on,
                    kind,
                    exempted,
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
                if c.exempted {
                    out.push_str(
                        "    claimed conflict_free: no stall derived (checked in simulation)\n",
                    );
                } else {
                    let loser = if c.winner == c.a { b } else { a };
                    let winner = rule_name(ast, c.winner);
                    out.push_str(&format!(
                        "    derived stall: {loser} fires only when {winner} is blocked or idle\n"
                    ));
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
